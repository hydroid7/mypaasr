//! Building images and replacing containers. Used by the daemon and by `mypaas server deploy`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::{Progress, owner};
use super::docker::{
    container_ip, docker, docker_quiet, docker_stream, ensure_network, image_exists, image_id, is_running,
    list_containers,
};
use crate::auth::{sha256_hex, unix_now};
use crate::config::{
    EffectiveLimits, Paths, ServerConfig, load_secrets, resolve_host_volume, resolve_named_volume, write_atomic,
};
use crate::spec::{Healthcheck, ServiceSpec, Strategy, VolumeSource, parse_dockerfile};

const READY_TIMEOUT: Duration = Duration::from_secs(60);
const NO_HEALTHCHECK_WAIT: Duration = Duration::from_secs(5);
const STOP_TIMEOUT_SECS: &str = "10";

/// Who started a deploy, for privilege checks and the deploy record.
#[derive(Debug, Clone)]
pub struct Caller {
    pub key_name: String,
    pub admin: bool,
}

/// What is stored in `services/<service>/current.json` and `previous.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployRecord {
    pub spec: ServiceSpec,
    pub deploy_id: String,
    pub image_id: String,
    pub time: u64,
    pub key_name: String,
}

/// Runtime settings derived from the spec and the server config.
struct Prepared {
    mounts: Vec<String>,
    env_lines: Vec<String>,
    cpu: f64,
    mem: u64,
    pids: u32,
    log_max_size: String,
    log_max_file: u32,
}

fn say(progress: &Progress, msg: impl Into<String>) {
    let _ = progress.send(msg.into());
}

pub fn service_network(service: &str) -> String {
    format!("mypaas-{service}")
}

pub fn shared_network(name: &str) -> String {
    format!("mypaas-net-{name}")
}

fn image_repo(service: &str) -> String {
    format!("mypaas-{service}")
}

fn new_deploy_id() -> String {
    format!("{}-{:04x}", unix_now(), rand::random::<u16>())
}

/// Deploys an unpacked bundle. With `expected_service`, the Dockerfile must name
/// exactly that service, so a ticket for one service cannot deploy another.
pub async fn deploy_dir(
    paths: &Paths,
    dir: &Path,
    caller: &Caller,
    expected_service: Option<&str>,
    progress: &Progress,
) -> Result<()> {
    let text = std::fs::read_to_string(dir.join(crate::bundle::BUNDLE_DOCKERFILE)).context("cannot read Dockerfile")?;
    let (spec, info) = parse_dockerfile(&text)?;
    if let Some(expected) = expected_service
        && spec.service != expected
    {
        bail!("the Dockerfile names service {:?}, but the ticket is for {expected:?}", spec.service);
    }
    let cfg = ServerConfig::load(paths)?;
    let limits = cfg.limits()?;
    let prepared = prepare(paths, &cfg, &limits, &spec, caller)?;

    let deploy_id = new_deploy_id();
    let image = format!("{}:{deploy_id}", image_repo(&spec.service));
    say(progress, format!("building {image}"));
    let mut args: Vec<String> = vec!["build".into(), "--progress".into(), "plain".into()];
    if info.pull {
        args.push("--pull".into());
    }
    args.extend([
        "--tag".into(),
        image.clone(),
        "--label".into(),
        format!("mypaas.service={}", spec.service),
        "--file".into(),
        dir.join("Dockerfile").to_string_lossy().into_owned(),
        dir.to_string_lossy().into_owned(),
    ]);
    docker_stream(&args, progress, Duration::from_millis(limits.build_timeout_ms)).await?;

    let result = async {
        prepare_ownership(paths, &cfg, &spec, &image, progress).await?;
        replace_containers(paths, &spec, &prepared, &image, &deploy_id, progress).await
    }
    .await;
    if let Err(e) = result {
        docker_quiet(&["rmi", &image]).await;
        return Err(e);
    }

    let record = DeployRecord {
        spec: spec.clone(),
        deploy_id: deploy_id.clone(),
        image_id: image_id(&image).await?,
        time: unix_now(),
        key_name: caller.key_name.clone(),
    };
    finish_deploy(paths, &spec.service, &image, &record).await?;
    for url in &spec.urls {
        say(progress, format!("serving {}", url.to_url()));
    }
    say(progress, format!("deployed {} ({deploy_id})", spec.service));
    Ok(())
}

/// Redeploys the `previous` image with the spec it was deployed with.
pub async fn rollback(paths: &Paths, service: &str, caller: &Caller, progress: &Progress) -> Result<()> {
    let dir = paths.service_dir(service);
    let previous: DeployRecord = read_record(&dir.join("previous.json"))?
        .with_context(|| format!("service {service} has no previous version to roll back to"))?;
    let image = format!("{}:previous", image_repo(service));
    if !image_exists(&image).await {
        bail!("the image {image} is gone, cannot roll back");
    }
    let cfg = ServerConfig::load(paths)?;
    let limits = cfg.limits()?;
    let prepared = prepare(paths, &cfg, &limits, &previous.spec, caller)?;
    say(progress, format!("rolling back {service} to {}", previous.deploy_id));
    prepare_ownership(paths, &cfg, &previous.spec, &image, progress).await?;
    replace_containers(paths, &previous.spec, &prepared, &image, &previous.deploy_id, progress).await?;

    docker(&["tag", &image, &format!("{}:current", image_repo(service))]).await?;
    docker_quiet(&["rmi", &image]).await;
    write_atomic(&dir.join("current.json"), &serde_json::to_vec_pretty(&previous)?, 0o644)?;
    std::fs::remove_file(dir.join("previous.json"))?;
    prune(service).await;
    say(progress, format!("rolled back {service}; no further rollback is possible"));
    Ok(())
}

/// Warns about root images and hands empty named volumes to the image's user.
async fn prepare_ownership(paths: &Paths, cfg: &ServerConfig, spec: &ServiceSpec, image: &str, progress: &Progress) -> Result<()> {
    let user = owner::image_user(image).await?;
    if user == owner::ImageUser::Root {
        say(progress, "warning: the image runs as root; add a USER line to the Dockerfile");
    }
    owner::prepare_volumes(paths, cfg, spec, &user, progress)
}

pub fn read_record(path: &Path) -> Result<Option<DeployRecord>> {
    match std::fs::read(path) {
        Ok(data) => Ok(Some(serde_json::from_slice(&data).with_context(|| format!("invalid {}", path.display()))?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Checks the spec against the caller's privileges and the server config.
fn prepare(
    paths: &Paths,
    cfg: &ServerConfig,
    limits: &EffectiveLimits,
    spec: &ServiceSpec,
    caller: &Caller,
) -> Result<Prepared> {
    let privileged = spec.privileged_features();
    if !privileged.is_empty() && !caller.admin {
        bail!("these settings need the admin scope: {}", privileged.join(", "));
    }
    for net in &spec.networks {
        if !cfg.networks.contains_key(net) {
            bail!("network {net:?} is not declared in the server's config.toml");
        }
        if !cfg.network_allows(net, &spec.service) {
            bail!("service {} is not a member of network {net:?}", spec.service);
        }
    }
    let cpu = spec.maxcpu.unwrap_or(limits.default_cpu);
    if cpu > limits.max_cpu {
        bail!("maxcpu {cpu} is above the server maximum of {}", limits.max_cpu);
    }
    let mem = spec.maxmem.unwrap_or(limits.default_mem);
    if mem > limits.max_mem {
        bail!("maxmem is above the server maximum of {}", cfg.maximums.mem);
    }
    if mem < 6 << 20 {
        bail!("maxmem must be at least 6m");
    }

    let mut mounts = Vec::new();
    for v in &spec.volumes {
        let host: PathBuf = match &v.source {
            VolumeSource::Named(name) => resolve_named_volume(paths, &spec.service, name)?,
            VolumeSource::Host(p) => resolve_host_volume(paths, cfg, p)?,
        };
        let host = host.to_str().context("volume path is not UTF-8")?;
        if host.contains([',', '"', '\n']) {
            bail!("volume path {host} contains characters that cannot be mounted");
        }
        let mut m = format!("type=bind,source={host},target={}", v.target);
        if v.readonly {
            m.push_str(",readonly");
        }
        mounts.push(m);
    }

    let secrets = load_secrets(paths, &spec.service)?;
    let mut env_lines = Vec::new();
    for e in &spec.env {
        let value = match &e.value {
            Some(v) => v.as_str(),
            None => secrets
                .get(&e.key)
                .map(String::as_str)
                .with_context(|| format!("secret {} is not set for service {}", e.key, spec.service))?,
        };
        if value.contains(['\n', '\r', '\0']) {
            bail!("the value of {} contains a line break, which cannot be passed to the container", e.key);
        }
        env_lines.push(format!("{}={value}", e.key));
    }

    check_url_conflicts(paths, spec)?;

    Ok(Prepared {
        mounts,
        env_lines,
        cpu,
        mem,
        pids: cfg.defaults.pids,
        log_max_size: cfg.defaults.log_max_size.clone(),
        log_max_file: cfg.defaults.log_max_file,
    })
}

/// Fails if another service already routes the same rule or router name.
fn check_url_conflicts(paths: &Paths, spec: &ServiceSpec) -> Result<()> {
    let mut names: Vec<String> = spec.urls.iter().map(|u| u.router_name()).collect();
    names.sort();
    if names.windows(2).any(|w| w[0] == w[1]) {
        bail!("two urls of this service map to the same router name");
    }
    let Ok(entries) = std::fs::read_dir(paths.services_dir()) else { return Ok(()) };
    for entry in entries.flatten() {
        let other = entry.file_name().to_string_lossy().into_owned();
        if other == spec.service {
            continue;
        }
        let Ok(Some(rec)) = read_record(&entry.path().join("current.json")) else { continue };
        for u in &spec.urls {
            for o in &rec.spec.urls {
                if u.rule() == o.rule() || u.router_name() == o.router_name() {
                    bail!("url {} is already used by service {other}", u.to_url());
                }
            }
        }
    }
    Ok(())
}

/// Labels that tell Traefik how to route to the container.
pub fn traefik_labels(spec: &ServiceSpec) -> Vec<String> {
    if spec.urls.is_empty() {
        return Vec::new();
    }
    let svc = format!("{}-service", spec.service);
    let mut l = vec![
        "traefik.enable=true".to_string(),
        format!("traefik.docker.network={}", service_network(&spec.service)),
        format!("traefik.http.services.{svc}.loadbalancer.server.port={}", spec.port),
    ];
    if spec.scale > 0
        && let Some(h) = &spec.healthcheck
    {
        let hc = format!("traefik.http.services.{svc}.loadbalancer.healthCheck");
        l.push(format!("{hc}.path={}", h.path));
        l.push(format!("{hc}.interval={}ms", h.interval_ms));
        l.push(format!("{hc}.timeout={}ms", h.timeout_ms));
    }
    for u in &spec.urls {
        let name = u.router_name();
        let rule = u.rule();
        let r = format!("traefik.http.routers.{name}-router");
        l.push(format!("{r}.rule={rule}"));
        l.push(format!("{r}.service={svc}"));
        if u.https {
            l.push(format!("{r}.entrypoints=web-secure"));
            l.push(format!("{r}.tls.certresolver=default"));
            l.push(format!("{r}.tls.options=intermediate@file"));
            l.push(format!("{r}.middlewares=hsts-header@file"));
            let rr = format!("traefik.http.routers.{name}-https-redirect");
            l.push(format!("{rr}.rule={rule}"));
            l.push(format!("{rr}.entrypoints=web"));
            l.push(format!("{rr}.middlewares=https-redirect@file"));
            l.push(format!("{rr}.service={svc}"));
        } else {
            l.push(format!("{r}.entrypoints=web"));
        }
    }
    l
}

/// Removes the env file when dropped, so secret values do not stay on disk.
struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

struct Run<'a> {
    spec: &'a ServiceSpec,
    prepared: &'a Prepared,
    image: &'a str,
    deploy_id: &'a str,
    env_file: PathBuf,
    progress: &'a Progress,
    created: Vec<String>,
}

impl Run<'_> {
    async fn create(&mut self, name: &str) -> Result<()> {
        let spec = self.spec;
        let p = self.prepared;
        let spec_hash = sha256_hex(&serde_json::to_vec(spec)?);
        let mut args: Vec<String> = vec![
            "create".into(),
            "--name".into(),
            name.into(),
            "--hostname".into(),
            spec.service.clone(),
            "--network".into(),
            service_network(&spec.service),
            "--restart".into(),
            "unless-stopped".into(),
            "--cap-drop".into(),
            "ALL".into(),
            "--security-opt".into(),
            "no-new-privileges".into(),
            "--pids-limit".into(),
            p.pids.to_string(),
            "--cpus".into(),
            p.cpu.to_string(),
            "--memory".into(),
            p.mem.to_string(),
            "--memory-swap".into(),
            p.mem.to_string(),
            "--log-driver".into(),
            "json-file".into(),
            "--log-opt".into(),
            format!("max-size={}", p.log_max_size),
            "--log-opt".into(),
            format!("max-file={}", p.log_max_file),
            "--env-file".into(),
            self.env_file.to_string_lossy().into_owned(),
            "--label".into(),
            format!("mypaas.service={}", spec.service),
            "--label".into(),
            format!("mypaas.spec={spec_hash}"),
            "--label".into(),
            format!("mypaas.deploy={}", self.deploy_id),
        ];
        for cap in &spec.cap_add {
            args.extend(["--cap-add".into(), cap.clone()]);
        }
        if spec.readonly {
            args.extend(["--read-only".into(), "--tmpfs".into(), "/tmp:rw,nosuid,nodev".into()]);
        }
        for pb in &spec.publish {
            let bind = match pb.bind {
                std::net::IpAddr::V6(v6) => format!("[{v6}]"),
                v4 => v4.to_string(),
            };
            let proto = if pb.udp { "udp" } else { "tcp" };
            args.extend(["--publish".into(), format!("{bind}:{}:{}/{proto}", pb.host_port, pb.container_port)]);
        }
        for m in &p.mounts {
            args.extend(["--mount".into(), m.clone()]);
        }
        for label in traefik_labels(spec) {
            args.extend(["--label".into(), label]);
        }
        args.push(self.image.into());
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        docker(&refs).await?;
        self.created.push(name.to_string());
        // `docker create` attaches one network; the shared ones are connected here.
        for net in &spec.networks {
            docker(&["network", "connect", "--alias", &spec.service, &shared_network(net), name]).await?;
        }
        Ok(())
    }

    async fn start_new(&mut self, name: &str) -> Result<()> {
        say(self.progress, format!("starting {name}"));
        self.create(name).await?;
        docker(&["start", name]).await?;
        if let Err(e) = self.wait_ready(name).await {
            if let Ok(logs) = docker(&["logs", "--tail", "20", name]).await {
                for line in logs.lines() {
                    say(self.progress, format!("  {line}"));
                }
            }
            return Err(e);
        }
        Ok(())
    }

    async fn wait_ready(&self, name: &str) -> Result<()> {
        let spec = self.spec;
        let Some(hc) = spec.healthcheck.as_ref().filter(|_| spec.scale > 0) else {
            tokio::time::sleep(NO_HEALTHCHECK_WAIT).await;
            if !is_running(name).await {
                bail!("container {name} stopped right after starting");
            }
            return Ok(());
        };
        let start = Instant::now();
        let mut last_err = String::from("no answer");
        while start.elapsed() < READY_TIMEOUT {
            if !is_running(name).await {
                bail!("container {name} stopped before it became healthy");
            }
            match probe(name, spec, hc).await {
                Ok(()) => {
                    say(self.progress, format!("{name} is healthy"));
                    return Ok(());
                }
                Err(e) => last_err = e,
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        bail!("{name} did not become healthy within {}s ({last_err})", READY_TIMEOUT.as_secs())
    }
}

/// One request to the container's healthcheck path; 2xx and 3xx count as healthy.
pub async fn probe(container: &str, spec: &ServiceSpec, hc: &Healthcheck) -> Result<(), String> {
    let ip = container_ip(container, &service_network(&spec.service)).await.map_err(|e| e.to_string())?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_millis(hc.timeout_ms))
        .build()
        .map_err(|e| e.to_string())?;
    match client.get(format!("http://{ip}:{}{}", spec.port, hc.path)).send().await {
        Ok(r) if r.status().is_success() || r.status().is_redirection() => Ok(()),
        Ok(r) => Err(format!("status {}", r.status())),
        Err(e) => Err(e.to_string()),
    }
}

fn new_names(spec: &ServiceSpec) -> Vec<String> {
    if spec.scale == 0 {
        vec![spec.service.clone()]
    } else {
        (1..=spec.scale).map(|i| format!("{}.{i}", spec.service)).collect()
    }
}

/// Swaps the running containers for new ones from `image`. On any failure the
/// new containers are removed and the old ones come back.
async fn replace_containers(
    paths: &Paths,
    spec: &ServiceSpec,
    prepared: &Prepared,
    image: &str,
    deploy_id: &str,
    progress: &Progress,
) -> Result<()> {
    ensure_network(&service_network(&spec.service), &format!("mypaas.service={}", spec.service)).await?;
    for net in &spec.networks {
        ensure_network(&shared_network(net), &format!("mypaas.network={net}")).await?;
    }

    let existing = list_containers(Some(&spec.service)).await?;
    for stale in existing.iter().filter(|c| c.name.contains(".old.")) {
        say(progress, format!("removing stale container {}", stale.name));
        docker(&["rm", "-f", &stale.name]).await?;
    }
    let mut current: Vec<&str> = existing.iter().filter(|c| !c.name.contains(".old.")).map(|c| c.name.as_str()).collect();
    current.sort();
    let stamp = unix_now();
    let mut olds: Vec<(String, String, bool)> = Vec::new(); // (original, renamed, was running)
    for (n, name) in current.iter().enumerate() {
        let renamed = format!("{name}.old.{stamp}.{n}");
        docker(&["rename", name, &renamed]).await?;
        let running = existing.iter().any(|c| c.name == *name && c.running);
        olds.push((name.to_string(), renamed, running));
    }

    let env_file = TempFile(paths.tmp().join(format!("env-{deploy_id}")));
    let mut content = prepared.env_lines.join("\n");
    content.push('\n');
    write_atomic(&env_file.0, content.as_bytes(), 0o600)?;

    let mut run = Run { spec, prepared, image, deploy_id, env_file: env_file.0.clone(), progress, created: Vec::new() };
    let result = run_strategy(&mut run, &olds).await;
    drop(env_file);

    match result {
        Ok(()) => {
            for (_, renamed, _) in &olds {
                docker_quiet(&["rm", "-f", renamed]).await;
            }
            Ok(())
        }
        Err(e) => {
            say(progress, format!("deploy failed: {e:#}"));
            say(progress, "restoring the previous containers");
            for name in &run.created {
                docker_quiet(&["rm", "-f", name]).await;
            }
            for (original, renamed, running) in &olds {
                docker_quiet(&["rename", renamed, original]).await;
                if *running {
                    docker_quiet(&["start", original]).await;
                }
            }
            Err(e)
        }
    }
}

async fn run_strategy(run: &mut Run<'_>, olds: &[(String, String, bool)]) -> Result<()> {
    let names = new_names(run.spec);
    let stop = |renamed: String| async move { docker(&["stop", "-t", STOP_TIMEOUT_SECS, &renamed]).await.map(|_| ()) };
    if run.spec.scale == 0 || run.spec.strategy == Strategy::Safe {
        for (_, renamed, _) in olds {
            say(run.progress, format!("stopping {renamed}"));
            stop(renamed.clone()).await?;
        }
        for name in &names {
            run.start_new(name).await?;
        }
        return Ok(());
    }
    // Roll: a new container must be ready before an old one stops.
    for (i, name) in names.iter().enumerate() {
        run.start_new(name).await?;
        if let Some((_, renamed, _)) = olds.get(i) {
            say(run.progress, format!("stopping {renamed}"));
            stop(renamed.clone()).await?;
        }
    }
    for (_, renamed, _) in olds.iter().skip(names.len()) {
        stop(renamed.clone()).await?;
    }
    Ok(())
}

/// Moves the image tags and deploy records forward and prunes this service's leftovers.
async fn finish_deploy(paths: &Paths, service: &str, image: &str, record: &DeployRecord) -> Result<()> {
    let repo = image_repo(service);
    let current = format!("{repo}:current");
    let previous = format!("{repo}:previous");
    if image_exists(&current).await {
        docker(&["tag", &current, &previous]).await?;
    }
    docker(&["tag", image, &current]).await?;
    docker_quiet(&["rmi", image]).await;

    let dir = paths.service_dir(service);
    std::fs::create_dir_all(&dir)?;
    if dir.join("current.json").exists() {
        std::fs::rename(dir.join("current.json"), dir.join("previous.json"))?;
    }
    write_atomic(&dir.join("current.json"), &serde_json::to_vec_pretty(record)?, 0o644)?;
    prune(service).await;
    Ok(())
}

/// Removes only dangling images that belong to this service.
async fn prune(service: &str) {
    docker_quiet(&["image", "prune", "--force", "--filter", &format!("label=mypaas.service={service}")]).await;
}

/// Removes a service's containers, networks and stored spec. Volumes stay.
pub async fn remove_service(paths: &Paths, service: &str, progress: &Progress) -> Result<()> {
    let containers = list_containers(Some(service)).await?;
    for c in &containers {
        say(progress, format!("removing {}", c.name));
        docker(&["rm", "-f", &c.name]).await?;
    }
    docker_quiet(&["network", "rm", &service_network(service)]).await;
    let repo = image_repo(service);
    docker_quiet(&["rmi", &format!("{repo}:current")]).await;
    docker_quiet(&["rmi", &format!("{repo}:previous")]).await;
    prune(service).await;
    let dir = paths.service_dir(service);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    Ok(())
}

/// After a crash in the middle of a deploy: services that have `.old.` containers
/// and no running new ones get their old containers back.
pub async fn recover(progress: &Progress) -> Result<()> {
    let all = list_containers(None).await?;
    let mut services: Vec<&str> = all.iter().map(|c| c.service.as_str()).collect();
    services.sort();
    services.dedup();
    for svc in services {
        let mine: Vec<_> = all.iter().filter(|c| c.service == svc).collect();
        let olds: Vec<_> = mine.iter().filter(|c| c.name.contains(".old.")).collect();
        if olds.is_empty() {
            continue;
        }
        if mine.iter().any(|c| !c.name.contains(".old.") && c.running) {
            say(progress, format!("service {svc} has leftover old containers next to running new ones; leaving them"));
            continue;
        }
        say(progress, format!("restoring old containers of {svc} after an interrupted deploy"));
        for c in mine.iter().filter(|c| !c.name.contains(".old.")) {
            docker_quiet(&["rm", "-f", &c.name]).await;
        }
        for old in olds {
            let original = &old.name[..old.name.find(".old.").unwrap()];
            if docker_quiet(&["rename", &old.name, original]).await {
                docker_quiet(&["start", original]).await;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::parse_spec;

    #[test]
    fn labels_for_https_url() {
        let spec = parse_spec("# mypaas.service=shop\n# mypaas.url=https://shop.example.com\n# mypaas.port=8080\n").unwrap();
        let l = traefik_labels(&spec);
        assert!(l.contains(&"traefik.docker.network=mypaas-shop".to_string()));
        assert!(l.contains(&"traefik.http.services.shop-service.loadbalancer.server.port=8080".to_string()));
        assert!(l.contains(&"traefik.http.routers.shop-example-com-router.rule=Host(`shop.example.com`)".to_string()));
        assert!(l.contains(&"traefik.http.routers.shop-example-com-router.tls.options=intermediate@file".to_string()));
        assert!(l.contains(&"traefik.http.routers.shop-example-com-https-redirect.middlewares=https-redirect@file".to_string()));
        assert!(!l.iter().any(|x| x.contains("healthCheck")));
    }

    #[test]
    fn labels_for_http_path_and_health() {
        let spec = parse_spec(
            "# mypaas.service=a\n# mypaas.url=http://x.org/api\n# mypaas.scale=2\n# mypaas.healthcheck=/h 10s 2s\n",
        )
        .unwrap();
        let l = traefik_labels(&spec);
        assert!(l.contains(&"traefik.http.routers.x-org-api-router.rule=Host(`x.org`) && PathPrefix(`/api`)".to_string()));
        assert!(l.contains(&"traefik.http.routers.x-org-api-router.entrypoints=web".to_string()));
        assert!(!l.iter().any(|x| x.contains("redirect") || x.contains("tls")));
        assert!(l.contains(&"traefik.http.services.a-service.loadbalancer.healthCheck.interval=10000ms".to_string()));
    }

    #[test]
    fn no_labels_without_url() {
        assert!(traefik_labels(&parse_spec("# mypaas.service=db\n").unwrap()).is_empty());
    }

    #[test]
    fn container_names() {
        assert_eq!(new_names(&parse_spec("# mypaas.service=a\n").unwrap()), vec!["a"]);
        assert_eq!(new_names(&parse_spec("# mypaas.service=a\n# mypaas.scale=2\n").unwrap()), vec!["a.1", "a.2"]);
    }

    #[test]
    fn privileges_and_networks() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { root: dir.path().to_path_buf() };
        let mut cfg = ServerConfig::default();
        cfg.networks.insert("db".into(), crate::config::SharedNetwork { members: vec!["shop-*".into()] });
        let limits = cfg.limits().unwrap();
        let user = Caller { key_name: "ci".into(), admin: false };
        let admin = Caller { key_name: "me".into(), admin: true };

        let s = parse_spec("# mypaas.service=shop-web\n# mypaas.network=db\n").unwrap();
        assert!(prepare(&paths, &cfg, &limits, &s, &user).is_ok());
        let s = parse_spec("# mypaas.service=blog\n# mypaas.network=db\n").unwrap();
        assert!(prepare(&paths, &cfg, &limits, &s, &admin).is_err());
        let s = parse_spec("# mypaas.service=a\n# mypaas.publish=0.0.0.0:80:80\n").unwrap();
        assert!(prepare(&paths, &cfg, &limits, &s, &user).is_err());
        assert!(prepare(&paths, &cfg, &limits, &s, &admin).is_ok());
        let s = parse_spec("# mypaas.service=a\n# mypaas.maxmem=64g\n").unwrap();
        assert!(prepare(&paths, &cfg, &limits, &s, &admin).is_err());
        let s = parse_spec("# mypaas.service=a\n# mypaas.env=TOKEN\n").unwrap();
        let err = prepare(&paths, &cfg, &limits, &s, &user).err().unwrap();
        assert!(format!("{err:#}").contains("TOKEN"));
        std::fs::create_dir_all(paths.secrets_dir()).unwrap();
        std::fs::write(paths.secrets_file("a"), "TOKEN = \"s3cret\"\n").unwrap();
        let p = prepare(&paths, &cfg, &limits, &s, &user).unwrap();
        assert_eq!(p.env_lines, vec!["TOKEN=s3cret"]);
        let s = parse_spec("# mypaas.service=a\n# mypaas.volume=data:/data\n").unwrap();
        let p = prepare(&paths, &cfg, &limits, &s, &user).unwrap();
        assert!(p.mounts[0].ends_with("volumes/a/data,target=/data"));
    }

    #[test]
    fn url_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { root: dir.path().to_path_buf() };
        let other = parse_spec("# mypaas.service=other\n# mypaas.url=https://x.com\n").unwrap();
        let rec = DeployRecord { spec: other, deploy_id: "1".into(), image_id: "i".into(), time: 0, key_name: "k".into() };
        std::fs::create_dir_all(paths.service_dir("other")).unwrap();
        std::fs::write(paths.service_dir("other").join("current.json"), serde_json::to_vec(&rec).unwrap()).unwrap();
        let mine = parse_spec("# mypaas.service=mine\n# mypaas.url=http://x.com\n").unwrap();
        assert!(format!("{:#}", check_url_conflicts(&paths, &mine).unwrap_err()).contains("other"));
        let mine = parse_spec("# mypaas.service=mine\n# mypaas.url=http://x.com/sub\n").unwrap();
        assert!(check_url_conflicts(&paths, &mine).is_ok());
        let same = parse_spec("# mypaas.service=other\n# mypaas.url=https://x.com\n").unwrap();
        assert!(check_url_conflicts(&paths, &same).is_ok());
    }
}

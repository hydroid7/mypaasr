//! `mypaas server …` commands that change local state: init, keys, Traefik, systemd.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::auth::{self, AuthorizedKey, Scope};
use super::sockproxy;
use crate::config::{Paths, ROUTER_CONTAINER, ServerConfig, TRAEFIK_VERSION, write_atomic};

pub const SYSTEMD_UNIT: &str = "/etc/systemd/system/mypaasd.service";
pub const PROXY_UNIT: &str = "/etc/systemd/system/mypaas-socket-proxy.service";
/// Where Traefik sees the proxy's socket directory.
const PROXY_MOUNT: &str = "/mypaas-run";

pub fn init(paths: &Paths, domain: &str, email: &str, key: Option<&str>) -> Result<()> {
    for dir in [
        paths.root.clone(),
        paths.secrets_dir(),
        paths.services_dir(),
        paths.volumes_dir(),
        paths.tmp(),
        sockproxy::socket_dir(paths),
        paths.root.join("docker-config"),
    ] {
        std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    set_mode(&paths.root, 0o700)?;
    set_mode(&paths.secrets_dir(), 0o700)?;

    let cfg = if paths.config().exists() {
        let mut cfg = ServerConfig::load(paths)?;
        cfg.domain = domain.to_string();
        cfg.email = email.to_string();
        cfg
    } else {
        ServerConfig { domain: domain.to_string(), email: email.to_string(), ..ServerConfig::default() }
    };
    cfg.validate(paths)?;
    if cfg.domain.is_empty() {
        bail!("an admin domain is required");
    }
    write_atomic(&paths.config(), toml::to_string_pretty(&cfg)?.as_bytes(), 0o600)?;

    if !paths.authorized_keys().exists() {
        write_atomic(
            &paths.authorized_keys(),
            b"# ssh-ed25519 AAAA... name scope=deploy:shop-*,read expires=2027-01-01\n",
            0o600,
        )?;
    }
    if !paths.acme_json().exists() {
        write_atomic(&paths.acme_json(), b"", 0o600)?;
    }
    write_traefik_files(paths, &cfg)?;

    if let Some(key) = key {
        let text = read_key_arg(key)?;
        add_key(paths, &text, "admin", &[Scope::Admin], None)?;
    }
    let has_keys = auth::parse_authorized_keys(&std::fs::read_to_string(paths.authorized_keys())?)?.len();
    if has_keys == 0 {
        eprintln!("warning: no key in authorized_keys yet; add one with `mypaas server key add FILE --name me --scope admin`");
    }

    // /run/systemd/system exists only while systemd is the running init.
    if Path::new("/run/systemd/system").is_dir() && unsafe { libc::geteuid() } == 0 {
        write_systemd_units(paths, &cfg)?;
        println!("wrote the systemd units; start everything with `mypaas server restart all`");
    } else {
        println!("not root or systemd is not running: skipped the systemd units");
    }
    println!("initialized {}", paths.root.display());
    Ok(())
}

fn set_mode(p: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).with_context(|| format!("chmod {}", p.display()))
}

fn read_key_arg(arg: &str) -> Result<String> {
    if arg == "-" {
        let mut s = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
        Ok(s)
    } else {
        std::fs::read_to_string(arg).with_context(|| format!("cannot read {arg}"))
    }
}

pub fn write_traefik_files(paths: &Paths, cfg: &ServerConfig) -> Result<()> {
    write_atomic(&paths.traefik_toml(), traefik_toml(cfg).as_bytes(), 0o644)?;
    write_atomic(&paths.staticroutes_toml(), staticroutes_toml(cfg).as_bytes(), 0o644)?;
    Ok(())
}

pub fn traefik_toml(cfg: &ServerConfig) -> String {
    let email = if cfg.email.is_empty() { String::new() } else { format!("email = \"{}\"\n", cfg.email) };
    let endpoint = if cfg.router.socket_proxy {
        format!("# Read-only access through `mypaas server socket-proxy`.\nendpoint = \"unix://{PROXY_MOUNT}/docker.sock\"\n")
    } else {
        String::new()
    };
    format!(
        r#"# Written by mypaas server init; changes are overwritten.

[log]
level = "WARN"

[entryPoints.web]
address = ":80"

[entryPoints.web-secure]
address = ":443"

# Uploads may take a few minutes; the daemon enforces its own limits.
[entryPoints.web-secure.transport.respondingTimeouts]
readTimeout = "600s"

[providers.docker]
{endpoint}exposedByDefault = false
watch = true

[providers.file]
filename = "/staticroutes.toml"
watch = true

[certificatesResolvers.default.acme]
{email}storage = "/acme.json"
keyType = "EC256"

[certificatesResolvers.default.acme.httpChallenge]
entryPoint = "web"
"#
    )
}

pub fn staticroutes_toml(cfg: &ServerConfig) -> String {
    let domain = &cfg.domain;
    let port = cfg.listen.rsplit(':').next().unwrap_or("88");
    format!(
        r#"# Written by mypaas server init; changes are overwritten.

# The upload route is exact, so it cannot also catch /v1/deploy-tickets.
# No buffering middleware here: the daemon must see the headers before the body.
[http.routers.mypaas-upload]
rule = "Host(`{domain}`) && Path(`/v1/deploy`)"
entryPoints = ["web-secure"]
service = "mypaas-daemon"
priority = 200
[http.routers.mypaas-upload.tls]
certResolver = "default"
options = "intermediate"

[http.routers.mypaas-api]
rule = "Host(`{domain}`) && PathPrefix(`/v1/`)"
entryPoints = ["web-secure"]
service = "mypaas-daemon"
priority = 100
[http.routers.mypaas-api.tls]
certResolver = "default"
options = "intermediate"

[http.routers.mypaas-api-redirect]
rule = "Host(`{domain}`)"
entryPoints = ["web"]
middlewares = ["https-redirect"]
service = "mypaas-daemon"

[http.services.mypaas-daemon.loadBalancer]
[[http.services.mypaas-daemon.loadBalancer.servers]]
url = "http://127.0.0.1:{port}"

[http.middlewares.https-redirect.redirectScheme]
scheme = "https"
permanent = true

[http.middlewares.hsts-header.headers]
stsSeconds = 63072000
stsIncludeSubdomains = true
stsPreload = true

[tls.options.intermediate]
minVersion = "VersionTLS12"
cipherSuites = [
  "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256",
  "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256",
  "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384",
  "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384",
  "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305",
  "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305",
]
"#
    )
}

/// Sandboxing shared by both units. The daemon is root-equivalent through the
/// Docker socket anyway; these limit what a bug in it can touch directly.
const HARDENING: &str = "NoNewPrivileges=yes
PrivateTmp=yes
PrivateDevices=yes
ProtectSystem=strict
ProtectHome=read-only
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectKernelLogs=yes
ProtectControlGroups=yes
ProtectClock=yes
ProtectHostname=yes
RestrictSUIDSGID=yes
RestrictRealtime=yes
RestrictNamespaces=yes
LockPersonality=yes
MemoryDenyWriteExecute=yes
SystemCallArchitectures=native
SystemCallFilter=@system-service";

pub fn systemd_unit(paths: &Paths, exe: &Path) -> String {
    format!(
        r#"[Unit]
Description=mypaas daemon
After=docker.service network-online.target
Requires=docker.service

[Service]
ExecStart={exe} server daemon
Environment=MYPAAS_HOME={root}
Environment=DOCKER_CONFIG={root}/docker-config
Restart=always
RestartSec=2
UMask=0022
{HARDENING}
ReadWritePaths={root}
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6
# Port 88, and reading and chowning volume directories owned by service users.
CapabilityBoundingSet=CAP_NET_BIND_SERVICE CAP_CHOWN CAP_FOWNER CAP_DAC_OVERRIDE CAP_DAC_READ_SEARCH

[Install]
WantedBy=multi-user.target
"#,
        exe = exe.display(),
        root = paths.root.display()
    )
}

pub fn proxy_unit(paths: &Paths, exe: &Path) -> String {
    format!(
        r#"[Unit]
Description=mypaas read-only Docker socket proxy for Traefik
After=docker.service
Requires=docker.service

[Service]
ExecStart={exe} server socket-proxy
Environment=MYPAAS_HOME={root}
Restart=always
RestartSec=2
UMask=0077
{HARDENING}
ReadWritePaths={run}
# Unix sockets only: no network at all, and no capabilities.
PrivateNetwork=yes
RestrictAddressFamilies=AF_UNIX
CapabilityBoundingSet=

[Install]
WantedBy=multi-user.target
"#,
        exe = exe.display(),
        root = paths.root.display(),
        run = sockproxy::socket_dir(paths).display()
    )
}

fn write_systemd_units(paths: &Paths, cfg: &ServerConfig) -> Result<()> {
    let exe = std::env::current_exe()?;
    write_atomic(Path::new(SYSTEMD_UNIT), systemd_unit(paths, &exe).as_bytes(), 0o644)?;
    write_atomic(Path::new(PROXY_UNIT), proxy_unit(paths, &exe).as_bytes(), 0o644)?;
    run("systemctl", &["daemon-reload"])?;
    run("systemctl", &["enable", "mypaasd"])?;
    if cfg.router.socket_proxy {
        run("systemctl", &["enable", "mypaas-socket-proxy"])?;
    } else {
        let _ = Command::new("systemctl").args(["disable", "--now", "mypaas-socket-proxy"]).status();
    }
    Ok(())
}

fn run(cmd: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(cmd).args(args).status().with_context(|| format!("cannot run {cmd}"))?;
    if !status.success() {
        bail!("{cmd} {} failed with {status}", args.join(" "));
    }
    Ok(())
}

pub fn restart(paths: &Paths, what: &str) -> Result<()> {
    match what {
        "all" => {
            restart_router(paths)?;
            restart_daemon()
        }
        "daemon" => restart_daemon(),
        "router" => restart_router(paths),
        _ => bail!("restart what? use all, daemon or router"),
    }
}

fn restart_daemon() -> Result<()> {
    run("systemctl", &["restart", "mypaasd"])?;
    println!("restarted mypaasd");
    Ok(())
}

fn restart_router(paths: &Paths) -> Result<()> {
    let cfg = ServerConfig::load(paths)?;
    write_traefik_files(paths, &cfg)?;
    if cfg.router.socket_proxy {
        run("systemctl", &["restart", "mypaas-socket-proxy"])?;
        println!("restarted mypaas-socket-proxy");
    }
    let image = format!("traefik:v{TRAEFIK_VERSION}");
    run("docker", &["pull", &image])?;
    let _ = Command::new("docker").args(["rm", "-f", ROUTER_CONTAINER]).output();
    let mount = |src: &Path, dst: &str, ro: bool| {
        format!("type=bind,source={},target={dst}{}", src.display(), if ro { ",readonly" } else { "" })
    };
    // The proxy's directory rather than its socket file, so a restarted proxy is
    // picked up. Without the proxy Traefik gets the raw socket.
    let docker_mount = if cfg.router.socket_proxy {
        mount(&sockproxy::socket_dir(paths), PROXY_MOUNT, true)
    } else {
        "type=bind,source=/var/run/docker.sock,target=/var/run/docker.sock,readonly".to_string()
    };
    // Host networking lets Traefik reach the daemon on loopback and every
    // service's bridge network; it binds ports 80 and 443 itself.
    run(
        "docker",
        &[
            "run",
            "--detach",
            "--name",
            ROUTER_CONTAINER,
            "--restart",
            "always",
            "--network",
            "host",
            "--log-opt",
            "max-size=10m",
            "--log-opt",
            "max-file=3",
            "--mount",
            &mount(&paths.traefik_toml(), "/traefik.toml", true),
            "--mount",
            &mount(&paths.staticroutes_toml(), "/staticroutes.toml", true),
            "--mount",
            &mount(&paths.acme_json(), "/acme.json", false),
            "--mount",
            &docker_mount,
            &image,
            "--configFile=/traefik.toml",
        ],
    )?;
    println!("started {ROUTER_CONTAINER} ({image})");
    Ok(())
}

pub fn add_key(paths: &Paths, public: &str, name: &str, scopes: &[Scope], expires: Option<u64>) -> Result<AuthorizedKey> {
    if !auth::is_valid_key_name(name) {
        bail!("invalid key name {name:?}: use letters, digits and - _ . @");
    }
    if scopes.is_empty() {
        bail!("give at least one --scope");
    }
    let first = public.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with('#')).context("no public key given")?;
    let (key, public_line, key_id) = auth::parse_public_key(first)?;
    let path = paths.authorized_keys();
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let keys = auth::parse_authorized_keys(&existing)?;
    if keys.iter().any(|k| k.name == name) {
        bail!("a key named {name} already exists");
    }
    if let Some(k) = keys.iter().find(|k| k.key_id == key_id) {
        bail!("this key is already authorized as {}", k.name);
    }
    let entry = AuthorizedKey { name: name.to_string(), key_id, key, public_line, scopes: scopes.to_vec(), expires };
    let mut text = existing;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&entry.to_line());
    text.push('\n');
    // Validate the result before it replaces the file the daemon reads.
    auth::parse_authorized_keys(&text)?;
    write_atomic(&path, text.as_bytes(), 0o600)?;
    Ok(entry)
}

pub fn list_keys(paths: &Paths) -> Result<Vec<AuthorizedKey>> {
    auth::parse_authorized_keys(&std::fs::read_to_string(paths.authorized_keys()).unwrap_or_default())
}

pub fn remove_key(paths: &Paths, name: &str) -> Result<()> {
    let path = paths.authorized_keys();
    let text = std::fs::read_to_string(&path)?;
    let mut found = false;
    let mut out = String::new();
    for line in text.lines() {
        let t = line.trim();
        if !t.is_empty() && !t.starts_with('#') && auth::parse_authorized_line(t)?.name == name {
            found = true;
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    if !found {
        bail!("no key named {name}");
    }
    write_atomic(&path, out.as_bytes(), 0o600)?;
    Ok(())
}

pub fn schedule_reboot(when: &str) -> Result<()> {
    if when.contains(['\n', '"', '\'']) || when.is_empty() {
        bail!("invalid time {when:?}");
    }
    let service = "[Unit]\nDescription=Reboot scheduled by mypaas\n\n[Service]\nType=oneshot\nExecStart=/usr/bin/systemctl reboot\n";
    let timer = format!(
        "[Unit]\nDescription=Reboot scheduled by mypaas\n\n[Timer]\nOnCalendar={when}\n\n[Install]\nWantedBy=timers.target\n"
    );
    write_atomic(Path::new("/etc/systemd/system/mypaas-reboot.service"), service.as_bytes(), 0o644)?;
    write_atomic(Path::new("/etc/systemd/system/mypaas-reboot.timer"), timer.as_bytes(), 0o644)?;
    run("systemctl", &["daemon-reload"])?;
    run("systemctl", &["enable", "--now", "mypaas-reboot.timer"])?;
    println!("the server reboots at {when}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::tests::test_key;

    #[test]
    fn key_add_list_remove() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { root: dir.path().to_path_buf() };
        let (_, public) = test_key();
        add_key(&paths, &public, "ci", &[Scope::parse("deploy:shop-*").unwrap()], Some(0)).unwrap();
        assert!(add_key(&paths, &public, "again", &[Scope::Read], None).is_err());
        let (_, other) = test_key();
        assert!(add_key(&paths, &other, "ci", &[Scope::Read], None).is_err());
        add_key(&paths, &other, "me", &[Scope::Admin], None).unwrap();
        let keys = list_keys(&paths).unwrap();
        assert_eq!(keys.len(), 2);
        remove_key(&paths, "ci").unwrap();
        assert_eq!(list_keys(&paths).unwrap()[0].name, "me");
        assert!(remove_key(&paths, "ci").is_err());
    }

    #[test]
    fn traefik_files_parse_as_toml() {
        let cfg = ServerConfig { domain: "paas.example.com".into(), email: "a@b.c".into(), ..ServerConfig::default() };
        let t: toml::Value = toml::from_str(&traefik_toml(&cfg)).unwrap();
        assert_eq!(t["certificatesResolvers"]["default"]["acme"]["email"].as_str(), Some("a@b.c"));
        let s: toml::Value = toml::from_str(&staticroutes_toml(&cfg)).unwrap();
        assert_eq!(
            s["http"]["routers"]["mypaas-upload"]["rule"].as_str(),
            Some("Host(`paas.example.com`) && Path(`/v1/deploy`)")
        );
        assert_eq!(s["http"]["services"]["mypaas-daemon"]["loadBalancer"]["servers"][0]["url"].as_str(), Some("http://127.0.0.1:88"));
        assert_eq!(t["providers"]["docker"]["endpoint"].as_str(), Some("unix:///mypaas-run/docker.sock"));
        let mut raw = cfg.clone();
        raw.router.socket_proxy = false;
        let t: toml::Value = toml::from_str(&traefik_toml(&raw)).unwrap();
        assert!(t["providers"]["docker"].get("endpoint").is_none());
    }

    #[test]
    fn init_is_rerunnable() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { root: dir.path().join("_mypaas") };
        let (_, public) = test_key();
        let keyfile = dir.path().join("k.pub");
        std::fs::write(&keyfile, &public).unwrap();
        init(&paths, "paas.example.com", "", Some(keyfile.to_str().unwrap())).unwrap();
        std::fs::write(paths.secrets_file("a"), "X = \"1\"\n").unwrap();
        init(&paths, "paas2.example.com", "", None).unwrap();
        assert_eq!(list_keys(&paths).unwrap().len(), 1);
        assert!(paths.secrets_file("a").exists());
        assert_eq!(ServerConfig::load(&paths).unwrap().domain, "paas2.example.com");
    }
}

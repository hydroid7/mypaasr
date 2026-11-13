//! `mypaas server …` commands that change local state: init, keys, Traefik, systemd.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::auth::{self, AuthorizedKey, Scope};
use super::{sockproxy, stats};
use crate::config::{Paths, ROUTER_CONTAINER, ServerConfig, traefik_image, write_atomic};

pub const SYSTEMD_UNIT: &str = "/etc/systemd/system/mypaasd.service";
pub const PROXY_UNIT: &str = "/etc/systemd/system/mypaas-socket-proxy.service";
pub const FIREWALL_UNIT: &str = "/etc/systemd/system/mypaas-firewall.service";
/// Where Traefik sees the proxy's socket directory.
const PROXY_MOUNT: &str = "/mypaas-run";

pub fn init(paths: &Paths, domain: &str, email: &str, key: Option<&str>, lan: bool) -> Result<()> {
    for dir in [
        paths.root.clone(),
        paths.secrets_dir(),
        paths.services_dir(),
        paths.volumes_dir(),
        paths.tmp(),
        sockproxy::socket_dir(paths),
        stats::log_dir(paths),
        paths.root.join("docker-config"),
    ] {
        std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    set_mode(&paths.root, 0o700)?;
    set_mode(&paths.secrets_dir(), 0o700)?;

    // Without --lan the existing [lan] table is kept, so a rerun cannot switch LAN mode off by accident.
    let mut cfg = if paths.config().exists() {
        let mut cfg = ServerConfig::load(paths)?;
        cfg.domain = domain.to_string();
        cfg.email = email.to_string();
        cfg
    } else {
        ServerConfig { domain: domain.to_string(), email: email.to_string(), ..ServerConfig::default() }
    };
    if lan {
        cfg.lan.enabled = true;
        if crate::server::lan::is_lan_host(&cfg.domain) {
            cfg.lan.plain_admin = true;
        }
    }
    cfg.validate(paths)?;
    if cfg.domain.is_empty() {
        bail!("an admin domain is required");
    }
    if lan && cfg.lan.plain_admin {
        eprintln!(
            "warning: LAN mode serves the admin API over unencrypted HTTP on port 80 ({}).\n\
             Every device on the LAN can read secrets, logs and deploy bundles, and whichever\n\
             device wins the name on the LAN receives the requests. Never forward port 80 from\n\
             the internet to this server.",
            cfg.domain
        );
    }
    if cfg.lan.plain_admin && cfg.router.api_allow.is_empty() {
        eprintln!(
            "warning: router.api_allow is empty, so any device on the LAN can reach the admin API.\n\
             Set it in config.toml to your LAN's network, for example api_allow = [\"192.168.1.0/24\"]."
        );
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
        add_init_key(paths, &text)?;
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
    let access_log = if cfg.stats.enabled {
        format!(
            r#"
# Only what the traffic stats need: no client addresses, paths or headers.
[accessLog]
filePath = "{log}/access.log"
format = "json"
bufferingSize = 100

[accessLog.fields]
defaultMode = "drop"

[accessLog.fields.names]
ServiceName = "keep"
DownstreamStatus = "keep"
Duration = "keep"
StartUTC = "keep"

[accessLog.fields.headers]
defaultMode = "drop"
"#,
            log = stats::LOG_MOUNT
        )
    } else {
        String::new()
    };
    let endpoint = if cfg.router.socket_proxy {
        format!("# Read-only access through `mypaas server socket-proxy`.\nendpoint = \"unix://{PROXY_MOUNT}/docker.sock\"\n")
    } else {
        String::new()
    };
    // In LAN mode uploads arrive on port 80, so port 80 needs the upload limit too.
    let (web_note, web_read) = if cfg.lan.plain_admin {
        (
            "# LAN mode: uploads arrive on this port, so it gets the same read timeout as web-secure.\n",
            cfg.router.read_timeout.as_str(),
        )
    } else {
        ("", "60s")
    };
    format!(
        r#"# Written by mypaas server init; changes are overwritten.

[log]
level = "WARN"

[entryPoints.web]
address = ":80"

{web_note}[entryPoints.web.transport.respondingTimeouts]
readTimeout = "{web_read}"
idleTimeout = "60s"

[entryPoints.web.http]
maxHeaderBytes = 32768

[entryPoints.web-secure]
address = ":443"

# Traefik has no separate header timeout, so readTimeout also limits how long
# a client may trickle headers. It must fit the largest upload; see
# router.read_timeout in config.toml.
[entryPoints.web-secure.transport.respondingTimeouts]
readTimeout = "{read_timeout}"
idleTimeout = "60s"

[entryPoints.web-secure.http]
maxHeaderBytes = 32768

[entryPoints.web-secure.http2]
maxConcurrentStreams = 100

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
{access_log}"#,
        read_timeout = cfg.router.read_timeout,
        web_note = web_note,
        web_read = web_read,
    )
}

/// TLS 1.2 suites with forward secrecy and AEAD only; TLS 1.3 suites are not configurable.
const CIPHERS: &str = r#"[
  "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256",
  "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256",
  "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384",
  "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384",
  "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305",
  "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305",
]"#;

pub fn staticroutes_toml(cfg: &ServerConfig) -> String {
    let domain = &cfg.domain;
    let port = cfg.listen.rsplit(':').next().unwrap_or("88");
    // Validated as addresses or CIDRs when the config is loaded.
    let (allow_mw, allow_def) = if cfg.router.api_allow.is_empty() {
        (String::new(), String::new())
    } else {
        let ranges: Vec<String> = cfg.router.api_allow.iter().map(|r| format!("\"{r}\"")).collect();
        (
            "\"mypaas-api-allow\", ".to_string(),
            format!(
                "# Only these networks reach the API (router.api_allow in config.toml).\n\
                 [http.middlewares.mypaas-api-allow.ipAllowList]\nsourceRange = [{}]\n\n",
                ranges.join(", ")
            ),
        )
    };
    // Plain-admin mode (LAN, docs/design.md): no certificate exists for the
    // .local name, so the routers stay on port 80 and nothing redirects to HTTPS.
    let (entry, plain_note, upload_tls, api_tls, redirect) = if cfg.lan.plain_admin {
        (
            "web",
            "# LAN mode: the admin API is plain HTTP on port 80, with no certificate and no HTTPS redirect.\n\n",
            String::new(),
            String::new(),
            String::new(),
        )
    } else {
        (
            "web-secure",
            "",
            "[http.routers.mypaas-upload.tls]\ncertResolver = \"default\"\noptions = \"intermediate\"\n".to_string(),
            "[http.routers.mypaas-api.tls]\ncertResolver = \"default\"\noptions = \"intermediate\"\n\n".to_string(),
            format!(
                "[http.routers.mypaas-api-redirect]\nrule = \"Host(`{domain}`)\"\nentryPoints = [\"web\"]\n\
                 middlewares = [\"https-redirect\"]\nservice = \"mypaas-daemon\"\npriority = 100000\n\n"
            ),
        )
    };
    format!(
        r#"# Written by mypaas server init; changes are overwritten.

{plain_note}# The upload route is exact, so it cannot also catch /v1/deploy-tickets.
# No buffering middleware here: the daemon must see the headers before the body.
[http.routers.mypaas-upload]
rule = "Host(`{domain}`) && Path(`/v1/deploy`)"
entryPoints = ["{entry}"]
service = "mypaas-daemon"
middlewares = [{allow_mw}"mypaas-api-limit", "mypaas-api-inflight"]
# Above any rule a service can produce (its default priority is the rule's
# length, and service URLs are bounded and may not use this domain anyway).
priority = 100001
{upload_tls}
[http.routers.mypaas-api]
rule = "Host(`{domain}`) && PathPrefix(`/v1/`)"
entryPoints = ["{entry}"]
service = "mypaas-daemon"
middlewares = [{allow_mw}"mypaas-api-limit", "mypaas-api-inflight"]
priority = 100000
{api_tls}{redirect}[http.services.mypaas-daemon.loadBalancer]
[[http.services.mypaas-daemon.loadBalancer.servers]]
url = "http://127.0.0.1:{port}"

{allow_def}# Per client address, in front of the daemon's own backoff. Neither buffers
# the body. Behind a CDN, both need a sourceCriterion for its header.
[http.middlewares.mypaas-api-limit.rateLimit]
average = 10
period = "1s"
burst = 50

[http.middlewares.mypaas-api-inflight.inFlightReq]
amount = 16

[http.middlewares.https-redirect.redirectScheme]
scheme = "https"
permanent = true

[http.middlewares.hsts-header.headers]
stsSeconds = 63072000
stsIncludeSubdomains = true
stsPreload = true

# Used for every connection whose name no router claims. sniStrict refuses
# the handshake instead of serving Traefik's default certificate, so scanners
# that connect by IP learn nothing.
[tls.options.default]
minVersion = "VersionTLS12"
sniStrict = {strict}
cipherSuites = {CIPHERS}

[tls.options.intermediate]
minVersion = "VersionTLS12"
sniStrict = {strict}
cipherSuites = {CIPHERS}
"#,
        strict = cfg.router.strict_sni,
        entry = entry,
        plain_note = plain_note,
        upload_tls = upload_tls,
        api_tls = api_tls,
        redirect = redirect,
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

pub fn systemd_unit(paths: &Paths, exe: &Path, cfg: &ServerConfig) -> String {
    // LAN mode lists interface addresses over netlink to know what to announce.
    let (families, netlink_note) = if cfg.lan.enabled {
        ("AF_UNIX AF_INET AF_INET6 AF_NETLINK", "# Interface addresses for mDNS announcements (LAN mode).\n")
    } else {
        ("AF_UNIX AF_INET AF_INET6", "")
    };
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
{netlink_note}RestrictAddressFamilies={families}
# Port 88, and reading and chowning volume directories owned by service users.
CapabilityBoundingSet=CAP_NET_BIND_SERVICE CAP_CHOWN CAP_FOWNER CAP_DAC_OVERRIDE CAP_DAC_READ_SEARCH

[Install]
WantedBy=multi-user.target
"#,
        exe = exe.display(),
        root = paths.root.display(),
        netlink_note = netlink_note,
        families = families,
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

/// Keeps containers, and builds, away from link-local addresses. On AWS, GCP
/// and Azure the instance metadata service at 169.254.169.254 hands out the
/// VM's cloud credentials to anyone who asks, so an SSRF bug in any hosted
/// app would become a cloud account compromise. Docker's DOCKER-USER chain sees
/// all traffic leaving container networks; the rule is put back whenever
/// Docker restarts.
pub fn firewall_unit() -> String {
    let rule4 = "DOCKER-USER -d 169.254.0.0/16 -j REJECT --reject-with icmp-port-unreachable";
    let rule6 = "DOCKER-USER -d fd00:ec2::254/128 -j REJECT";
    format!(
        r#"[Unit]
Description=mypaas: block containers from the cloud metadata service
After=docker.service
Requires=docker.service
PartOf=docker.service

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/bin/sh -c 'iptables -C {rule4} 2>/dev/null || iptables -I {rule4}'
ExecStart=-/bin/sh -c 'ip6tables -C {rule6} 2>/dev/null || ip6tables -I {rule6}'
ExecStop=-/bin/sh -c 'iptables -D {rule4}; ip6tables -D {rule6}'

[Install]
WantedBy=docker.service
"#
    )
}

fn write_systemd_units(paths: &Paths, cfg: &ServerConfig) -> Result<()> {
    let exe = std::env::current_exe()?;
    write_atomic(Path::new(SYSTEMD_UNIT), systemd_unit(paths, &exe, cfg).as_bytes(), 0o644)?;
    write_atomic(Path::new(PROXY_UNIT), proxy_unit(paths, &exe).as_bytes(), 0o644)?;
    write_atomic(Path::new(FIREWALL_UNIT), firewall_unit().as_bytes(), 0o644)?;
    run("systemctl", &["daemon-reload"])?;
    if let Err(e) = run("systemctl", &["enable", "--now", "mypaas-firewall"]) {
        // For example Docker with its nftables backend, which has no DOCKER-USER chain.
        eprintln!("warning: could not block the cloud metadata service for containers: {e:#}");
    }
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
    let image = traefik_image();
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
    let mut args: Vec<&str> = vec!["run", "--detach", "--name", ROUTER_CONTAINER, "--restart", "always", "--network", "host"];
    args.extend_from_slice(ROUTER_HARDENING);
    let (config, routes, acme, logs) = (
        mount(&paths.traefik_toml(), "/traefik.toml", true),
        mount(&paths.staticroutes_toml(), "/staticroutes.toml", true),
        mount(&paths.acme_json(), "/acme.json", false),
        mount(&stats::log_dir(paths), stats::LOG_MOUNT, false),
    );
    args.extend_from_slice(&[
        "--log-opt", "max-size=10m", "--log-opt", "max-file=3",
        "--mount", &config, "--mount", &routes, "--mount", &acme, "--mount", &docker_mount, "--mount", &logs,
        &image, "--configFile=/traefik.toml",
    ]);
    run("docker", &args)?;
    println!("started {ROUTER_CONTAINER} ({image})");
    Ok(())
}

/// How Traefik runs. It is the only process on a public port, so it gets as
/// little as possible: no capabilities except binding ports 80 and 443, no
/// privilege escalation, a read-only image, and memory and process limits.
/// `tests/docker.rs` starts its Traefik with the same flags.
pub const ROUTER_HARDENING: &[&str] = &[
    "--cap-drop",
    "ALL",
    "--cap-add",
    "NET_BIND_SERVICE",
    "--security-opt",
    "no-new-privileges",
    "--read-only",
    "--tmpfs",
    "/tmp:rw,nosuid,nodev,noexec,size=16m",
    "--memory",
    "512m",
    "--pids-limit",
    "512",
];

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

/// The `--key` of `server init`: added as `admin`, or `admin-2`, `admin-3`… if
/// that name is taken, and left alone if it is already authorized, so running
/// init again never fails.
fn add_init_key(paths: &Paths, public: &str) -> Result<()> {
    let first = public.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with('#')).context("no public key given")?;
    let (_, _, key_id) = auth::parse_public_key(first)?;
    let keys = list_keys(paths)?;
    if let Some(k) = keys.iter().find(|k| k.key_id == key_id) {
        println!("key {} is already authorized as {}", key_id, k.name);
        return Ok(());
    }
    let name = std::iter::once("admin".to_string())
        .chain((2..).map(|n| format!("admin-{n}")))
        .find(|n| !keys.iter().any(|k| &k.name == n))
        .unwrap();
    let k = add_key(paths, public, &name, &[Scope::Admin], None)?;
    println!("added {} ({}) with the admin scope", k.name, k.key_id);
    Ok(())
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
        assert_eq!(t["accessLog"]["fields"]["defaultMode"].as_str(), Some("drop"), "privacy: drop by default");
        assert_eq!(t["accessLog"]["fields"]["names"].as_table().unwrap().len(), 4);
        // Internet-facing limits.
        let ws = &t["entryPoints"]["web-secure"];
        assert_eq!(ws["transport"]["respondingTimeouts"]["readTimeout"].as_str(), Some("300s"));
        assert_eq!(ws["http"]["maxHeaderBytes"].as_integer(), Some(32768));
        assert_eq!(t["entryPoints"]["web"]["transport"]["respondingTimeouts"]["readTimeout"].as_str(), Some("60s"));
        let routers = &s["http"]["routers"];
        for r in ["mypaas-upload", "mypaas-api"] {
            let mw: Vec<&str> = routers[r]["middlewares"].as_array().unwrap().iter().map(|m| m.as_str().unwrap()).collect();
            assert_eq!(mw, ["mypaas-api-limit", "mypaas-api-inflight"], "{r}");
            assert!(routers[r]["priority"].as_integer().unwrap() >= 100_000, "{r} outranks any service rule");
        }
        assert!(s["http"]["middlewares"]["mypaas-api-limit"]["rateLimit"]["average"].is_integer());
        assert_eq!(s["tls"]["options"]["default"]["sniStrict"].as_bool(), Some(true));
        assert_eq!(s["tls"]["options"]["default"]["minVersion"].as_str(), Some("VersionTLS12"));
        assert_eq!(s["tls"]["options"]["intermediate"]["cipherSuites"].as_array().unwrap().len(), 6);
        assert!(s["http"]["middlewares"].get("mypaas-api-allow").is_none(), "no allowlist unless configured");
        let mut only = cfg.clone();
        only.router.api_allow = vec!["203.0.113.0/24".into(), "2001:db8::/32".into()];
        let s: toml::Value = toml::from_str(&staticroutes_toml(&only)).unwrap();
        let ranges = s["http"]["middlewares"]["mypaas-api-allow"]["ipAllowList"]["sourceRange"].as_array().unwrap();
        assert_eq!(ranges.len(), 2);
        assert_eq!(s["http"]["routers"]["mypaas-upload"]["middlewares"][0].as_str(), Some("mypaas-api-allow"), "checked first");
        let mut lax = cfg.clone();
        lax.router.strict_sni = false;
        let s: toml::Value = toml::from_str(&staticroutes_toml(&lax)).unwrap();
        assert_eq!(s["tls"]["options"]["default"]["sniStrict"].as_bool(), Some(false));

        let mut raw = cfg.clone();
        raw.router.socket_proxy = false;
        raw.stats.enabled = false;
        let t: toml::Value = toml::from_str(&traefik_toml(&raw)).unwrap();
        assert!(t["providers"]["docker"].get("endpoint").is_none());
        assert!(t.get("accessLog").is_none());
    }

    #[test]
    fn firewall_unit_blocks_metadata_idempotently() {
        let u = firewall_unit();
        assert!(u.contains("-C DOCKER-USER -d 169.254.0.0/16"), "checks before inserting, so restarts add no duplicates");
        assert!(u.contains("-I DOCKER-USER -d 169.254.0.0/16"));
        assert!(u.contains("PartOf=docker.service"), "re-applied when Docker restarts");
    }

    #[test]
    fn init_is_rerunnable() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { root: dir.path().join("_mypaas") };
        let (_, public) = test_key();
        let keyfile = dir.path().join("k.pub");
        std::fs::write(&keyfile, &public).unwrap();
        init(&paths, "paas.example.com", "", Some(keyfile.to_str().unwrap()), false).unwrap();
        std::fs::write(paths.secrets_file("a"), "X = \"1\"\n").unwrap();
        init(&paths, "paas2.example.com", "", None, false).unwrap();
        assert_eq!(list_keys(&paths).unwrap().len(), 1);
        // The same --key again is a no-op, a second one gets its own name.
        init(&paths, "paas2.example.com", "", Some(keyfile.to_str().unwrap()), false).unwrap();
        assert_eq!(list_keys(&paths).unwrap().len(), 1);
        let (_, other) = test_key();
        std::fs::write(&keyfile, &other).unwrap();
        init(&paths, "paas2.example.com", "", Some(keyfile.to_str().unwrap()), false).unwrap();
        let names: Vec<String> = list_keys(&paths).unwrap().into_iter().map(|k| k.name).collect();
        assert_eq!(names, vec!["admin", "admin-2"]);
        assert!(paths.secrets_file("a").exists());
        assert_eq!(ServerConfig::load(&paths).unwrap().domain, "paas2.example.com");
    }

    fn base_cfg() -> ServerConfig {
        ServerConfig { domain: "paas.example.com".into(), email: "a@b.c".into(), ..ServerConfig::default() }
    }

    fn plain_cfg() -> ServerConfig {
        let mut cfg = base_cfg();
        cfg.domain = "paas.local".into();
        cfg.lan.enabled = true;
        cfg.lan.plain_admin = true;
        cfg
    }

    fn unit(cfg: &ServerConfig) -> String {
        systemd_unit(&Paths { root: "/var/lib/mypaas/_mypaas".into() }, Path::new("/usr/bin/mypaas"), cfg)
    }

    #[test]
    fn default_config_output_is_unchanged() {
        let cfg = base_cfg();
        assert_eq!(traefik_toml(&cfg), include_str!("testdata/traefik.golden.toml"));
        assert_eq!(staticroutes_toml(&cfg), include_str!("testdata/staticroutes.golden.toml"));
        assert_eq!(unit(&cfg), include_str!("testdata/mypaasd.golden.service"), "the unit without LAN mode");
    }

    #[test]
    fn plain_admin_keeps_the_admin_api_on_port_80() {
        let mut cfg = plain_cfg();
        cfg.router.api_allow = vec!["192.168.1.0/24".into()];
        let t: toml::Value = toml::from_str(&traefik_toml(&cfg)).unwrap();
        let s: toml::Value = toml::from_str(&staticroutes_toml(&cfg)).unwrap();
        assert_eq!(
            t["entryPoints"]["web"]["transport"]["respondingTimeouts"]["readTimeout"].as_str(),
            Some(cfg.router.read_timeout.as_str()),
            "uploads arrive on port 80, so it needs the upload read timeout"
        );
        let routers = &s["http"]["routers"];
        for r in ["mypaas-upload", "mypaas-api"] {
            assert_eq!(routers[r]["entryPoints"][0].as_str(), Some("web"), "{r}");
            assert!(routers[r].get("tls").is_none(), "{r} has no certificate to present");
            let mw: Vec<&str> = routers[r]["middlewares"].as_array().unwrap().iter().map(|m| m.as_str().unwrap()).collect();
            assert_eq!(mw, ["mypaas-api-allow", "mypaas-api-limit", "mypaas-api-inflight"], "{r}");
        }
        assert_eq!(routers["mypaas-upload"]["priority"].as_integer(), Some(100_001));
        assert_eq!(routers["mypaas-api"]["priority"].as_integer(), Some(100_000));
        assert!(routers.get("mypaas-api-redirect").is_none(), "would redirect to HTTPS, which has no certificate");
        assert!(!staticroutes_toml(&cfg).contains("web-secure"));
    }

    #[test]
    fn netlink_is_allowed_only_in_lan_mode() {
        let mut cfg = base_cfg();
        assert!(!unit(&cfg).contains("AF_NETLINK"), "no netlink without LAN mode");
        cfg.lan.enabled = true;
        assert!(unit(&cfg).contains("RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK\n"));
    }

    #[cfg(feature = "lan")]
    #[test]
    fn init_with_lan_on_a_local_domain_serves_plain_admin_and_rerun_keeps_it() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { root: dir.path().join("_mypaas") };
        init(&paths, "paas.local", "", None, true).unwrap();
        let cfg = ServerConfig::load(&paths).unwrap();
        assert!(cfg.lan.enabled && cfg.lan.plain_admin);
        init(&paths, "paas.local", "", None, false).unwrap();
        let cfg = ServerConfig::load(&paths).unwrap();
        assert!(cfg.lan.enabled && cfg.lan.plain_admin, "a rerun without --lan keeps [lan]");
    }

    #[cfg(feature = "lan")]
    #[test]
    fn init_with_lan_on_a_public_domain_enables_lan_without_plain_admin() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { root: dir.path().join("_mypaas") };
        init(&paths, "paas.example.com", "", None, true).unwrap();
        let cfg = ServerConfig::load(&paths).unwrap();
        assert!(cfg.lan.enabled && !cfg.lan.plain_admin, "plain HTTP needs a .local name");
    }
}

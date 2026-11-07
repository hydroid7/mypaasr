//! End-to-end tests against a real Docker daemon, driving the real binary as
//! client and daemon. They need Docker and the busybox image, so they are
//! ignored by default:
//!
//!     cargo test --test docker -- --ignored
//!
//! Every test uses its own MYPAAS_HOME, port and service names, and removes the
//! containers, images and networks it created, even when it fails.

use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_mypaas");
const BASE_IMAGE: &str = "busybox:1.36";

fn docker(args: &[&str]) -> String {
    let out = Command::new("docker").args(args).output().expect("docker");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn docker_ok(args: &[&str]) -> bool {
    Command::new("docker").args(args).output().map(|o| o.status.success()).unwrap_or(false)
}

struct Env {
    dir: tempfile::TempDir,
    home: PathBuf,
    port: u16,
    private_key: String,
    daemon: Option<Child>,
    /// Prefix that makes this test's names unique.
    tag: String,
    networks: Vec<String>,
    foreign: Vec<String>,
}

impl Env {
    /// `shared` lists shared networks as (name, member services without the tag).
    fn new(shared: &[(&str, &[&str])]) -> Env {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let tag = format!("it{:06x}", rand::random::<u32>() & 0xff_ffff);
        let st = Command::new(BIN)
            .args(["server", "init", "--domain", "test.local", "--email", ""])
            .env("MYPAAS_HOME", &home)
            .output()
            .unwrap();
        assert!(st.status.success(), "{}", String::from_utf8_lossy(&st.stderr));
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let mut cfg = std::fs::read_to_string(home.join("config.toml")).unwrap();
        cfg = cfg.replace("listen = \"127.0.0.1:88\"", &format!("listen = \"127.0.0.1:{port}\""));
        let mut networks = Vec::new();
        for (name, members) in shared {
            let net = format!("{tag}-{name}");
            let members: Vec<String> = members.iter().map(|m| format!("\"{tag}-{m}\"")).collect();
            cfg.push_str(&format!("\n[networks.{net}]\nmembers = [{}]\n", members.join(", ")));
            networks.push(format!("mypaas-net-{net}"));
        }
        std::fs::write(home.join("config.toml"), cfg).unwrap();

        let generated = Command::new(BIN).args(["key", "gen"]).output().unwrap();
        let private_key = String::from_utf8(generated.stdout).unwrap();
        let public = String::from_utf8(generated.stderr).unwrap().lines().last().unwrap().to_string();
        let mut add = Command::new(BIN)
            .args(["server", "key", "add", "-", "--name", "test", "--scope", "admin"])
            .env("MYPAAS_HOME", &home)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        add.stdin.take().unwrap().write_all(public.as_bytes()).unwrap();
        assert!(add.wait().unwrap().success());

        let mut env = Env { dir, home, port, private_key, daemon: None, tag, networks, foreign: Vec::new() };
        env.start_daemon();
        env
    }

    fn start_daemon(&mut self) {
        let log = std::fs::File::create(self.dir.path().join(format!("daemon-{}.log", rand::random::<u16>()))).unwrap();
        let child = Command::new(BIN)
            .args(["server", "daemon"])
            .env("MYPAAS_HOME", &self.home)
            .stderr(log)
            .spawn()
            .unwrap();
        self.daemon = Some(child);
        let deadline = Instant::now() + Duration::from_secs(10);
        while std::net::TcpStream::connect(("127.0.0.1", self.port)).is_err() {
            assert!(Instant::now() < deadline, "daemon did not start");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn stop_daemon(&mut self) {
        if let Some(mut c) = self.daemon.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    fn svc(&self, name: &str) -> String {
        format!("{}-{name}", self.tag)
    }

    /// Writes a project whose Dockerfile starts with the given mypaas lines.
    fn project(&self, name: &str, mypaas: &str, body: &str) -> PathBuf {
        let dir = self.dir.path().join(format!("proj-{name}-{}", rand::random::<u16>()));
        std::fs::create_dir_all(&dir).unwrap();
        let df = format!("# mypaas.service={}\n{mypaas}\nFROM {BASE_IMAGE}\n{body}\n", self.svc(name));
        std::fs::write(dir.join("Dockerfile"), df).unwrap();
        dir
    }

    fn client(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .args(["-r", &format!("http://127.0.0.1:{}", self.port)])
            .args(args)
            .env("MYPAAS_PRIVATE_KEY", &self.private_key)
            .env("XDG_CONFIG_HOME", self.dir.path().join("cfg"))
            .output()
            .unwrap()
    }

    fn push(&self, dir: &Path) -> (i32, Vec<String>) {
        let out = self.client(&["push", dir.to_str().unwrap()]);
        let lines = String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).collect();
        (out.status.code().unwrap_or(-1), lines)
    }

    fn containers(&self, name: &str) -> Vec<String> {
        let filter = format!("label=mypaas.service={}", self.svc(name));
        let mut v: Vec<String> = docker(&["ps", "-a", "--filter", &filter, "--format", "{{.Names}} {{.State}}"])
            .lines()
            .map(str::to_string)
            .collect();
        v.sort();
        v
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        self.stop_daemon();
        let mut ids = docker(&["ps", "-aq", "--filter", "label=mypaas.service"]);
        ids = ids
            .lines()
            .filter(|id| docker(&["inspect", "--format", "{{index .Config.Labels \"mypaas.service\"}}", id]).starts_with(&self.tag))
            .collect::<Vec<_>>()
            .join(" ");
        for id in ids.split_whitespace() {
            docker(&["rm", "-f", id]);
        }
        for line in docker(&["images", "--format", "{{.Repository}}:{{.Tag}}"]).lines() {
            if line.starts_with(&format!("mypaas-{}", self.tag)) {
                docker(&["rmi", "-f", line]);
            }
        }
        for line in docker(&["network", "ls", "--format", "{{.Name}}"]).lines() {
            if line.starts_with(&format!("mypaas-{}", self.tag)) || self.networks.iter().any(|n| n == line) {
                docker(&["network", "rm", line]);
            }
        }
        for f in &self.foreign {
            docker(&["rm", "-f", f]);
            docker(&["rmi", "-f", f]);
        }
    }
}

fn http_from(container: &str, url: &str) -> Option<String> {
    let out = Command::new("docker").args(["exec", container, "wget", "-q", "-T", "3", "-O", "-", url]).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn serve(content: &str) -> String {
    format!("RUN mkdir /www && echo {content} > /www/index.html\nUSER 65534\nCMD [\"httpd\", \"-f\", \"-p\", \"8080\", \"-h\", \"/www\"]")
}

fn position(lines: &[String], needle: &str) -> usize {
    lines.iter().position(|l| l.contains(needle)).unwrap_or_else(|| panic!("no line with {needle:?} in:\n{}", lines.join("\n")))
}

#[test]
#[ignore = "needs docker"]
fn roll_deploy_keeps_capacity_and_rollback_works_once() {
    let env = Env::new(&[]);
    let svc = env.svc("web");
    let conf = "# mypaas.port=8080\n# mypaas.scale=2 roll\n# mypaas.healthcheck=/ 2s 1s";
    let (code, out) = env.push(&env.project("web", conf, &serve("v1")));
    assert_eq!(code, 0, "{out:?}");
    assert_eq!(env.containers("web"), vec![format!("{svc}.1 running"), format!("{svc}.2 running")]);

    let (code, out) = env.push(&env.project("web", conf, &serve("v2")));
    assert_eq!(code, 0, "{out:?}");
    // Roll: each new container is healthy before an old one stops, one at a time.
    let order = [
        position(&out, &format!("starting {svc}.1")),
        position(&out, &format!("{svc}.1 is healthy")),
        position(&out, &format!("stopping {svc}.1.old.")),
        position(&out, &format!("starting {svc}.2")),
        position(&out, &format!("{svc}.2 is healthy")),
        position(&out, &format!("stopping {svc}.2.old.")),
    ];
    assert!(order.windows(2).all(|w| w[0] < w[1]), "wrong order {order:?}:\n{}", out.join("\n"));
    assert_eq!(http_from(&format!("{svc}.1"), "http://127.0.0.1:8080/").as_deref(), Some("v2"));
    assert!(env.containers("web").iter().all(|c| !c.contains(".old.")), "old containers are removed");

    let out = env.client(&["rollback", &svc]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    assert_eq!(http_from(&format!("{svc}.2"), "http://127.0.0.1:8080/").as_deref(), Some("v1"));
    // Exactly one previous version is kept.
    let out = env.client(&["rollback", &svc]);
    assert_eq!(out.status.code(), Some(4));
    let tags = docker(&["images", &format!("mypaas-{svc}"), "--format", "{{.Tag}}"]);
    assert_eq!(tags, "current", "no deploy-id or stale tags are left behind");
}

#[test]
#[ignore = "needs docker"]
fn failed_deploy_restores_the_old_container() {
    let env = Env::new(&[]);
    let svc = env.svc("app");
    let (code, out) = env.push(&env.project("app", "# mypaas.port=8080", &serve("good")));
    assert_eq!(code, 0, "{out:?}");
    let image_before = docker(&["image", "inspect", "--format", "{{.Id}}", &format!("mypaas-{svc}:current")]);
    let id_before = docker(&["inspect", "--format", "{{.Id}}", &svc]);

    // The build succeeds, the container dies right away.
    let (code, out) = env.push(&env.project("app", "# mypaas.port=8080", "CMD [\"sh\", \"-c\", \"exit 1\"]"));
    assert_eq!(code, 4, "{out:?}");
    assert!(out.iter().any(|l| l.contains("restoring the previous containers")));
    assert_eq!(env.containers("app"), vec![format!("{svc} running")]);
    assert_eq!(docker(&["inspect", "--format", "{{.Id}}", &svc]), id_before, "the very same container is back");
    assert_eq!(http_from(&svc, "http://127.0.0.1:8080/").as_deref(), Some("good"));
    let image_after = docker(&["image", "inspect", "--format", "{{.Id}}", &format!("mypaas-{svc}:current")]);
    assert_eq!(image_after, image_before, "tags stay as they were");
    let tags = docker(&["images", &format!("mypaas-{svc}"), "--format", "{{.Tag}}"]);
    assert_eq!(tags, "current", "the failed image is removed");

    // A failed build changes nothing at all.
    let (code, _) = env.push(&env.project("app", "# mypaas.port=8080", "RUN exit 3"));
    assert_eq!(code, 4);
    assert_eq!(docker(&["inspect", "--format", "{{.Id}}", &svc]), id_before);
}

#[test]
#[ignore = "needs docker"]
fn safe_mode_never_mixes_versions() {
    let env = Env::new(&[]);
    let svc = env.svc("safe");
    let conf = "# mypaas.port=8080\n# mypaas.scale=2 safe";
    assert_eq!(env.push(&env.project("safe", conf, &serve("v1"))).0, 0);
    let (code, out) = env.push(&env.project("safe", conf, &serve("v2")));
    assert_eq!(code, 0, "{out:?}");
    let last_stop = out.iter().rposition(|l| l.starts_with("stopping ")).unwrap();
    let first_start = position(&out, &format!("starting {svc}.1"));
    assert!(last_stop < first_start, "all old containers stop before a new one starts:\n{}", out.join("\n"));
}

/// The original pruned every stopped container and dangling image on the host.
#[test]
#[ignore = "needs docker"]
fn pruning_leaves_other_peoples_objects_alone() {
    let mut env = Env::new(&[]);
    let foreign_container = format!("{}-foreign", env.tag);
    assert!(docker_ok(&["create", "--name", &foreign_container, BASE_IMAGE, "true"]));
    let ctx = env.dir.path().join("foreign-image");
    std::fs::create_dir_all(&ctx).unwrap();
    std::fs::write(ctx.join("Dockerfile"), format!("FROM {BASE_IMAGE}\nRUN echo {}\n", env.tag)).unwrap();
    // Untagged, so it is dangling right away.
    let foreign_image = docker(&["build", "-q", ctx.to_str().unwrap()]);
    assert!(!foreign_image.is_empty());
    env.foreign = vec![foreign_container.clone(), foreign_image.clone()];

    for v in ["v1", "v2", "v3"] {
        assert_eq!(env.push(&env.project("p", "# mypaas.port=8080", &serve(v))).0, 0);
    }
    assert!(docker_ok(&["inspect", &foreign_container]), "a stopped foreign container survives");
    assert!(docker_ok(&["image", "inspect", &foreign_image]), "a dangling foreign image survives");
    let tags = docker(&["images", &format!("mypaas-{}", env.svc("p")), "--format", "{{.Tag}}"]);
    let mut tags: Vec<&str> = tags.lines().collect();
    tags.sort();
    assert_eq!(tags, vec!["current", "previous"], "our own old images are pruned");
}

#[test]
#[ignore = "needs docker"]
fn daemon_restores_containers_after_an_interrupted_deploy() {
    let mut env = Env::new(&[]);
    let svc = env.svc("crash");
    assert_eq!(env.push(&env.project("crash", "# mypaas.port=8080", &serve("alive"))).0, 0);
    // What a crash between "rename the old containers" and "start the new ones" leaves.
    env.stop_daemon();
    assert!(docker_ok(&["stop", "-t", "1", &svc]));
    assert!(docker_ok(&["rename", &svc, &format!("{svc}.old.123.0")]));
    env.start_daemon();
    let deadline = Instant::now() + Duration::from_secs(10);
    while env.containers("crash") != vec![format!("{svc} running")] {
        assert!(Instant::now() < deadline, "not restored: {:?}", env.containers("crash"));
        std::thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(http_from(&svc, "http://127.0.0.1:8080/").as_deref(), Some("alive"));
}

/// Phase 3 exit test, first half: services are isolated unless a shared
/// network is declared on the server and they are listed as members.
#[test]
#[ignore = "needs docker"]
fn services_are_isolated_unless_they_share_a_network() {
    let env = Env::new(&[("db", &["db", "member"])]);
    let net = format!("{}-db", env.tag);
    let db = env.svc("db");
    let shared = format!("# mypaas.port=8080\n# mypaas.network={net}");
    assert_eq!(env.push(&env.project("db", &shared, &serve("data"))).0, 0);
    assert_eq!(env.push(&env.project("member", &shared, &serve("m"))).0, 0);
    assert_eq!(env.push(&env.project("loner", "# mypaas.port=8080", &serve("l"))).0, 0);

    let member = env.svc("member");
    let loner = env.svc("loner");
    assert_eq!(http_from(&member, &format!("http://{db}:8080/")).as_deref(), Some("data"), "members reach each other by name");
    let db_ip = docker(&["inspect", "--format", &format!("{{{{(index .NetworkSettings.Networks \"mypaas-net-{net}\").IPAddress}}}}"), &db]);
    assert_eq!(http_from(&loner, &format!("http://{db_ip}:8080/")), None, "a non-member cannot reach it even by address");
    let own_ip = docker(&["inspect", "--format", &format!("{{{{(index .NetworkSettings.Networks \"mypaas-{member}\").IPAddress}}}}"), &member]);
    assert_eq!(http_from(&loner, &format!("http://{own_ip}:8080/")), None, "nor another service's own network");

    // Writing the same comment does not make a service a member.
    let (code, out) = env.push(&env.project("intruder", &shared, &serve("x")));
    assert_eq!(code, 4);
    assert!(out.iter().any(|l| l.contains("not a member")), "{out:?}");
    assert!(env.containers("intruder").is_empty());
}

#[test]
#[ignore = "needs docker"]
fn containers_run_hardened() {
    let env = Env::new(&[]);
    let svc = env.svc("hard");
    assert_eq!(env.push(&env.project("hard", "# mypaas.port=8080\n# mypaas.readonly=true", &serve("h"))).0, 0);
    let f = "{{.HostConfig.CapDrop}}|{{.HostConfig.CapAdd}}|{{.HostConfig.SecurityOpt}}|{{.HostConfig.Privileged}}|{{.HostConfig.PidsLimit}}|{{.HostConfig.ReadonlyRootfs}}|{{.HostConfig.NetworkMode}}|{{.HostConfig.Memory}}";
    assert_eq!(docker(&["inspect", "--format", f, &svc]), format!("[ALL]|[]|[no-new-privileges]|false|512|true|mypaas-{svc}|536870912"));
    let out = Command::new("docker").args(["exec", &svc, "touch", "/etc/x"]).output().unwrap();
    assert!(!out.status.success(), "the root filesystem is read-only");
    assert!(Command::new("docker").args(["exec", &svc, "touch", "/tmp/x"]).output().unwrap().status.success(), "/tmp is writable");
}

/// Runs the socket proxy and a real Traefik for this test, on loopback ports.
struct Router {
    proxy: Child,
    container: String,
    web_port: u16,
}

impl Router {
    fn start(env: &Env) -> Router {
        let proxy = Command::new(BIN)
            .args(["server", "socket-proxy"])
            .env("MYPAAS_HOME", &env.home)
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let web_port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let tls_port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let cfg = std::fs::read_to_string(env.home.join("traefik.toml"))
            .unwrap()
            .replace("address = \":80\"", &format!("address = \"127.0.0.1:{web_port}\""))
            .replace("address = \":443\"", &format!("address = \"127.0.0.1:{tls_port}\""));
        let cfg_path = env.dir.path().join("traefik-test.toml");
        std::fs::write(&cfg_path, cfg).unwrap();
        let container = format!("{}-router", env.tag);
        let mount = |src: &Path, dst: &str, ro: bool| {
            format!("type=bind,source={},target={dst}{}", src.display(), if ro { ",readonly" } else { "" })
        };
        let ok = docker_ok(&[
            "run", "-d", "--name", &container, "--network", "host",
            "--mount", &mount(&cfg_path, "/traefik.toml", true),
            "--mount", &mount(&env.home.join("staticroutes.toml"), "/staticroutes.toml", true),
            "--mount", &mount(&env.home.join("run"), "/mypaas-run", true),
            "--mount", &mount(&env.home.join("logs"), "/mypaas-logs", false),
            "traefik:v3.6.1", "--configFile=/traefik.toml",
        ]);
        assert!(ok, "cannot start traefik");
        let deadline = Instant::now() + Duration::from_secs(15);
        while std::net::TcpStream::connect(("127.0.0.1", web_port)).is_err() {
            assert!(Instant::now() < deadline, "traefik did not start");
            std::thread::sleep(Duration::from_millis(100));
        }
        Router { proxy, container, web_port }
    }

    /// GET through Traefik; returns the status code.
    fn get(&self, host: &str, path: &str) -> u16 {
        use std::io::Read;
        let mut s = std::net::TcpStream::connect(("127.0.0.1", self.web_port)).unwrap();
        write!(s, "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").unwrap();
        let mut buf = String::new();
        let _ = s.read_to_string(&mut buf);
        buf.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0)
    }
}

impl Drop for Router {
    fn drop(&mut self) {
        let _ = self.proxy.kill();
        let _ = self.proxy.wait();
        docker(&["rm", "-f", &self.container]);
    }
}

/// Phase 4 exit test: a known mix of traffic through the real Traefik shows up
/// in the stats with exact counts and plausible latencies, and the access log
/// holds no client addresses or paths.
#[test]
#[ignore = "needs docker and the traefik:v3.6.1 image"]
fn traffic_stats_match_the_requests_sent() {
    let env = Env::new(&[]);
    let svc = env.svc("traffic");
    let host = format!("{svc}.test.local");
    let body = "RUN mkdir -p /www/cgi-bin && echo ok > /www/index.html \
        && printf '#!/bin/sh\\nsleep 0.3\\nprintf \"Content-Type: text/plain\\\\r\\\\n\\\\r\\\\nslow\"\\n' > /www/cgi-bin/slow \
        && printf '#!/bin/sh\\nprintf \"HTTP/1.1 500 Internal Server Error\\\\r\\\\nContent-Type: text/plain\\\\r\\\\n\\\\r\\\\nboom\"\\n' > /www/cgi-bin/fail \
        && chmod +x /www/cgi-bin/*\nUSER 65534\nCMD [\"httpd\", \"-f\", \"-p\", \"8080\", \"-h\", \"/www\"]";
    let conf = format!("# mypaas.port=8080\n# mypaas.url=http://{host}");
    let router = Router::start(&env);
    let (code, out) = env.push(&env.project("traffic", &conf, body));
    assert_eq!(code, 0, "{out:?}");

    // Traefik picks the new container up from Docker events.
    let deadline = Instant::now() + Duration::from_secs(20);
    while router.get(&host, "/") != 200 {
        assert!(Instant::now() < deadline, "traefik never routed to the service");
        std::thread::sleep(Duration::from_millis(300));
    }
    let sent = [("/", 200, 19), ("/missing", 404, 5), ("/cgi-bin/fail", 500, 3), ("/cgi-bin/slow", 200, 4)];
    for (path, want, n) in sent {
        for _ in 0..n {
            assert_eq!(router.get(&host, path), want, "{path}");
        }
    }
    let expected = 1 + 19 + 5 + 3 + 4;

    let deadline = Instant::now() + Duration::from_secs(40);
    let stats = loop {
        let out = env.client(&["--json", "stats", &svc, "--since", "2h"]);
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
        if v["total"]["requests"] == expected {
            break v;
        }
        assert!(Instant::now() < deadline, "stats never reached {expected}: {v}");
        std::thread::sleep(Duration::from_secs(1));
    };
    let total = &stats["total"];
    assert_eq!(total["status"]["2xx"], 24);
    assert_eq!(total["status"]["4xx"], 5);
    assert_eq!(total["status"]["5xx"], 3);
    let p50 = total["p50_ms"].as_f64().unwrap();
    let p95 = total["p95_ms"].as_f64().unwrap();
    assert!(p50 < 100.0, "p50 {p50}ms should be a fast request");
    assert!((250.0..1000.0).contains(&p95), "p95 {p95}ms should be a 300ms request");

    let ls: serde_json::Value = serde_json::from_slice(&env.client(&["--json", "ls"]).stdout).unwrap();
    let row = ls["services"].as_array().unwrap().iter().find(|s| s["service"] == svc.as_str()).unwrap().clone();
    assert_eq!(row["requests_1h"], expected);

    let log = std::fs::read_to_string(env.home.join("logs/access.log")).unwrap();
    assert!(log.lines().count() >= expected as usize);
    for forbidden in ["127.0.0.1", "/missing", "cgi-bin", "ClientHost", "RequestPath", "User-Agent"] {
        assert!(!log.contains(forbidden), "the access log must not contain {forbidden:?}");
    }
}

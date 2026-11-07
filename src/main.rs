mod auth;
mod bundle;
mod client;
mod config;
mod server;
mod spec;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use client::Output;
use config::{Paths, TRAEFIK_VERSION, TRAEFIK_DIGEST};

#[derive(Parser)]
#[command(name = "mypaas", version, about = "Deploy Dockerfiles to your own server over one small HTTPS API")]
struct Cli {
    /// Print JSON instead of tables, one object per line for streams.
    #[arg(long, global = true)]
    json: bool,
    #[arg(long, global = true)]
    no_color: bool,
    /// Print only errors and results.
    #[arg(short, long, global = true)]
    quiet: bool,
    /// Remote name or https://domain; defaults to $MYPAAS_REMOTE or the only remote.
    #[arg(short, long, global = true)]
    remote: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Pack and deploy a project directory or a Dockerfile.
    Push {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Only list the files and sizes that would be uploaded.
        #[arg(long)]
        dry_run: bool,
    },
    /// One row per service: replicas, health, age, CPU, memory, URLs.
    Ls,
    /// Containers, URLs, networks, volumes and the last deploy of a service.
    Status { service: String },
    /// Show a service's logs.
    Logs {
        service: String,
        /// Keep streaming new lines.
        #[arg(short, long)]
        follow: bool,
        /// Number of lines from the end (default 200, or 50 with -f).
        #[arg(long)]
        tail: Option<u64>,
        /// Only lines newer than this, e.g. 10m or 2h.
        #[arg(long)]
        since: Option<String>,
        /// Only this replica, e.g. 2 for <service>.2.
        #[arg(long)]
        container: Option<u32>,
    },
    /// Live view of all services; q quits.
    Top,
    /// Requests, status classes and p50/p95 latency from the access log.
    Stats {
        service: String,
        /// How far back, e.g. 24h, 7d, 90d.
        #[arg(long, default_value = "24h")]
        since: String,
        /// Bucket size; hour up to 2 days, day beyond, unless given.
        #[arg(long, value_parser = ["hour", "day"])]
        by: Option<String>,
    },
    /// Manage a service's secrets; values are never shown.
    #[command(subcommand)]
    Secret(SecretCmd),
    /// Check the key, TLS, the clock and that the server accepts signed requests.
    Doctor,
    /// Redeploy the previous version of a service.
    Rollback { service: String },
    /// Remove a service's containers, networks and stored spec; volumes stay.
    Rm { service: String },
    /// Manage servers.
    #[command(subcommand)]
    Remote(RemoteCmd),
    /// Manage the signing key.
    #[command(subcommand)]
    Key(KeyCmd),
    /// Commands that run on the server itself, as root.
    #[command(subcommand)]
    Server(ServerCmd),
    /// Print the version.
    Version,
}

#[derive(Subcommand)]
enum SecretCmd {
    /// Set a secret; the value comes from stdin or a hidden prompt.
    Set { service: String, key: String },
    /// List secret names.
    Ls { service: String },
    /// Remove a secret.
    Rm { service: String, key: String },
}

#[derive(Subcommand)]
enum RemoteCmd {
    Add { name: String, domain: String },
    Ls,
    Rm { name: String },
}

#[derive(Subcommand)]
enum KeyCmd {
    /// Create a new key (passphrase optional), or select an existing OpenSSH key.
    Init {
        #[arg(long)]
        existing: Option<PathBuf>,
        /// Store the new key unencrypted (no passphrase prompt).
        #[arg(long, conflicts_with = "existing")]
        no_passphrase: bool,
    },
    /// Print a passwordless key for MYPAAS_PRIVATE_KEY in CI.
    Gen,
    /// Print the public line for authorized_keys.
    Show,
}

#[derive(Subcommand)]
enum ServerCmd {
    /// Create ~/_mypaas, the Traefik files and the systemd units. Safe to run again.
    Init {
        #[arg(long)]
        domain: Option<String>,
        #[arg(long)]
        email: Option<String>,
        /// Public key file (or - for stdin) to add as the first admin key.
        #[arg(long)]
        key: Option<String>,
    },
    /// Restart the daemon, the router (Traefik) or both.
    Restart {
        #[arg(value_parser = ["all", "daemon", "router"])]
        what: String,
    },
    /// Deploy a local directory without authentication, through the same code as the API.
    Deploy { path: PathBuf },
    #[command(subcommand)]
    Key(ServerKeyCmd),
    /// Reboot the server on a schedule (systemd OnCalendar syntax).
    #[command(alias = "schedule_reboot")]
    ScheduleReboot {
        #[arg(default_value = "Sun 06:00:00")]
        when: String,
    },
    /// Run the HTTP daemon (started by systemd).
    #[command(hide = true)]
    Daemon,
    /// Run the read-only Docker socket proxy for Traefik (started by systemd).
    #[command(hide = true)]
    SocketProxy,
    Version,
}

#[derive(Subcommand)]
enum ServerKeyCmd {
    /// Authorize a public key.
    Add {
        /// Public key file, or - for stdin.
        file: String,
        #[arg(long)]
        name: String,
        /// read, logs, deploy:<glob>, secrets:<glob>, rm:<glob> or admin; repeatable.
        #[arg(long = "scope", required = true)]
        scopes: Vec<String>,
        /// YYYY-MM-DD
        #[arg(long)]
        expires: Option<String>,
    },
    Ls,
    Rm { name: String },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if !matches!(cli.command, Command::Server(_)) {
        // `mypaas ls | head` should end quietly. Rust ignores SIGPIPE so writes
        // return errors instead; println! turns those into a panic. Network
        // writes use MSG_NOSIGNAL and are unaffected. The server keeps ignoring
        // it so a closed pipe can never kill the daemon.
        // SAFETY: called before any thread is started.
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    }
    let out = Output {
        json: cli.json,
        quiet: cli.quiet,
        color: !cli.no_color && std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal(),
    };
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(client::EXIT_ERROR);
        }
    };
    match runtime.block_on(run(cli, out)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            if let Some(x) = e.downcast_ref::<client::Exit>() {
                eprintln!("error: {x}");
                return ExitCode::from(x.code);
            }
            eprintln!("error: {e:#}");
            ExitCode::from(client::EXIT_ERROR)
        }
    }
}

async fn run(cli: Cli, out: Output) -> Result<()> {
    let remote = cli.remote.as_deref();
    match cli.command {
        Command::Push { path, dry_run } => client::push(remote, &path, dry_run, out).await,
        Command::Ls => client::ops::ls(remote, out).await,
        Command::Status { service } => client::ops::status(remote, &service, out).await,
        Command::Logs { service, follow, tail, since, container } => {
            client::ops::logs(remote, &service, client::ops::LogArgs { follow, tail, since, container }, out).await
        }
        Command::Top => client::ops::top(remote, out).await,
        Command::Stats { service, since, by } => client::ops::stats(remote, &service, &since, by.as_deref(), out).await,
        Command::Secret(SecretCmd::Set { service, key }) => client::ops::secret_set(remote, &service, &key, out).await,
        Command::Secret(SecretCmd::Ls { service }) => client::ops::secret_ls(remote, &service, out).await,
        Command::Secret(SecretCmd::Rm { service, key }) => client::ops::secret_rm(remote, &service, &key, out).await,
        Command::Doctor => client::ops::doctor(remote, out).await,
        Command::Rollback { service } => client::rollback(remote, &service, out).await,
        Command::Rm { service } => client::rm(remote, &service, out).await,
        Command::Remote(RemoteCmd::Add { name, domain }) => client::remote_add(&name, &domain),
        Command::Remote(RemoteCmd::Ls) => client::remote_ls(out),
        Command::Remote(RemoteCmd::Rm { name }) => client::remote_rm(&name),
        Command::Key(KeyCmd::Init { existing, no_passphrase }) => client::key_init(existing.as_deref(), no_passphrase),
        Command::Key(KeyCmd::Gen) => client::key_gen(),
        Command::Key(KeyCmd::Show) => client::key_show(),
        Command::Version => {
            println!("mypaas {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Command::Server(cmd) => run_server(cmd).await,
    }
}

fn prompt(question: &str) -> Result<String> {
    eprint!("{question}");
    let mut s = String::new();
    std::io::stdin().read_line(&mut s)?;
    Ok(s.trim().to_string())
}

async fn run_server(cmd: ServerCmd) -> Result<()> {
    let paths = Paths::from_env()?;
    match cmd {
        ServerCmd::Init { domain, email, key } => {
            let domain = match domain {
                Some(d) => d,
                None => prompt("admin domain (e.g. paas.example.com): ")?,
            };
            let email = match email {
                Some(e) => e,
                None => prompt("Let's Encrypt account email (optional, Enter to skip): ")?,
            };
            server::setup::init(&paths, &domain, &email, key.as_deref())
        }
        ServerCmd::Restart { what } => server::setup::restart(&paths, &what),
        ServerCmd::Deploy { path } => local_deploy(&paths, &path).await,
        ServerCmd::Key(ServerKeyCmd::Add { file, name, scopes, expires }) => {
            let text = if file == "-" {
                let mut s = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
                s
            } else {
                std::fs::read_to_string(&file).with_context(|| format!("cannot read {file}"))?
            };
            let scopes = scopes
                .iter()
                .flat_map(|s| s.split(','))
                .map(auth::Scope::parse)
                .collect::<Result<Vec<_>>>()?;
            let expires = expires.as_deref().map(auth::parse_date).transpose()?;
            let k = server::setup::add_key(&paths, &text, &name, &scopes, expires)?;
            println!("added {} ({})", k.name, k.key_id);
            Ok(())
        }
        ServerCmd::Key(ServerKeyCmd::Ls) => {
            for k in server::setup::list_keys(&paths)? {
                let scopes: Vec<String> = k.scopes.iter().map(|s| s.to_string()).collect();
                let expires = k.expires.map(auth::format_date).unwrap_or_else(|| "never".into());
                println!("{:<20} {}  {}  expires {expires}", k.name, k.key_id, scopes.join(","));
            }
            Ok(())
        }
        ServerCmd::Key(ServerKeyCmd::Rm { name }) => {
            server::setup::remove_key(&paths, &name)?;
            println!("removed {name}; the daemon stops accepting it within 5 seconds");
            Ok(())
        }
        ServerCmd::ScheduleReboot { when } => server::setup::schedule_reboot(&when),
        ServerCmd::Daemon => {
            init_logging();
            server::daemon::run(paths).await
        }
        ServerCmd::SocketProxy => {
            init_logging();
            server::sockproxy::run(&paths, std::path::Path::new(server::sockproxy::UPSTREAM)).await
        }
        ServerCmd::Version => {
            println!("mypaas {} (traefik {TRAEFIK_VERSION}, {TRAEFIK_DIGEST})", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}

/// Logs to stderr (journald under systemd); `MYPAAS_LOG=debug` for more detail.
fn init_logging() {
    let level = match std::env::var("MYPAAS_LOG").as_deref() {
        Ok("debug") => tracing::Level::DEBUG,
        Ok("warn") => tracing::Level::WARN,
        _ => tracing::Level::INFO,
    };
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .with_max_level(level)
        .init();
}

/// `mypaas server deploy`: pack, unpack and deploy like an upload, as admin.
async fn local_deploy(paths: &Paths, path: &std::path::Path) -> Result<()> {
    let (dir, dockerfile) = client::resolve_project(path)?;
    let lock = server::DeployLock::try_acquire(paths)?.context("another deploy is running")?;
    let cfg = config::ServerConfig::load(paths)?;
    let l = cfg.limits()?;
    let bundle = client::pack_project(&dir, &dockerfile)?;
    let unpacked = paths.tmp().join(format!("local-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&unpacked);
    std::fs::create_dir_all(&unpacked)?;
    let limits = bundle::UnpackLimits { max_unpacked: l.max_unpacked, max_files: l.max_files, max_depth: l.max_depth };
    let result = async {
        bundle::unpack(&bundle.path, &unpacked, limits)?;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let printer = tokio::spawn(async move {
            while let Some(line) = rx.recv().await {
                println!("{line}");
            }
        });
        let caller = server::deploy::Caller { key_name: "local".into(), admin: true };
        let r = server::deploy::deploy_dir(paths, &unpacked, &caller, None, &tx).await;
        drop(tx);
        let _ = printer.await;
        r
    }
    .await;
    let _ = std::fs::remove_dir_all(&unpacked);
    drop(lock);
    result.map_err(|e| client::Exit { code: client::EXIT_DEPLOY_FAILED, msg: format!("{e:#}") }.into())
}

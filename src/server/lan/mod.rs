//! LAN mode: services that declare a `.local` URL get that name announced over
//! mDNS by the daemon, so any device on the LAN can open it. See the "LAN mode"
//! section of docs/design.md.
//!
//! Everything LAN-specific lives in this directory. The pure parts (host
//! derivation, validation, the reconcile loop) are always compiled; only the
//! mDNS backend needs the `lan` Cargo feature.

// Without the feature nothing drives the loop, but it stays compiled and tested.
#[cfg_attr(not(feature = "lan"), allow(dead_code))]
mod announce;
mod hosts;
#[cfg(feature = "lan")]
mod mdns;

use std::collections::BTreeMap;
use std::sync::mpsc;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{Paths, ServerConfig};

pub use hosts::{check_urls, is_lan_host};

/// Whether this binary can announce names at all.
pub const COMPILED: bool = cfg!(feature = "lan");

/// What happened to one announced name, as written to `lan/state.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", tag = "state", content = "detail")]
pub enum HostState {
    Announced,
    /// Another device on the LAN already uses the name. Traefik routes by the
    /// exact Host, so a renamed announcement would be useless; it is withdrawn.
    Conflict,
    Error(String),
}

impl HostState {
    pub fn label(&self) -> String {
        match self {
            HostState::Announced => "announced".into(),
            HostState::Conflict => "CONFLICT: name in use by another device".into(),
            HostState::Error(e) => format!("error: {e}"),
        }
    }
}

/// `~/_mypaas/lan/state.json`, read by `ls`, `status` and `doctor`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanState {
    pub hosts: BTreeMap<String, HostState>,
    pub updated: u64,
}

pub fn state_file(paths: &Paths) -> std::path::PathBuf {
    paths.root.join("lan").join("state.json")
}

/// The last state the announcer wrote, or `None` when LAN mode is off or has
/// not run yet.
pub fn read_state(paths: &Paths) -> Option<LanState> {
    let data = std::fs::read(state_file(paths)).ok()?;
    serde_json::from_slice(&data).ok()
}

/// Checks `[lan]` when the config is loaded.
pub fn validate_config(cfg: &ServerConfig) -> Result<()> {
    let lan = &cfg.lan;
    if lan.enabled && !COMPILED {
        bail!("lan.enabled is true, but this mypaas was built without the `lan` feature");
    }
    for name in &lan.interfaces {
        if name.is_empty() || name.len() > 15 || !name.bytes().all(|c| c.is_ascii_alphanumeric() || b"-_.@".contains(&c)) {
            bail!("lan.interfaces entry {name:?} is not an interface name");
        }
    }
    for pattern in &lan.allow {
        if !pattern.ends_with(".local") || !pattern.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"-.*?".contains(&c)) {
            bail!("lan.allow entry {pattern:?} must be a glob of .local names, like *.local or shop-*.local");
        }
    }
    if lan.plain_admin {
        if !lan.enabled {
            bail!("lan.plain_admin needs lan.enabled = true");
        }
        if !is_lan_host(&cfg.domain) {
            bail!("lan.plain_admin needs a .local admin domain, not {:?}", cfg.domain);
        }
    }
    Ok(())
}

/// Whether the client may talk plain HTTP to this remote host: `.local` names
/// only, and only in a binary that has LAN mode.
pub fn plain_remote_allowed(host: &str) -> bool {
    COMPILED && is_lan_host(host)
}

/// Lets deploy handlers wake the announcer after `current.json` changed.
#[derive(Clone, Default)]
pub struct LanHandle {
    tx: Option<mpsc::Sender<()>>,
}

impl LanHandle {
    /// Never blocks; a lost poke only delays the next reconcile by a few seconds.
    pub fn poke(&self) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(());
        }
    }
}

/// Starts the announcer thread when `lan.enabled`; otherwise does nothing at
/// all (no thread, no socket, no state file).
pub fn start(paths: &Paths, cfg: &ServerConfig) -> Result<LanHandle> {
    if !cfg.lan.enabled {
        // A state file from when LAN mode was on would make `ls` claim names
        // are still announced.
        let _ = std::fs::remove_file(state_file(paths));
        return Ok(LanHandle::default());
    }
    if cfg.lan.plain_admin && cfg.router.api_allow.is_empty() {
        tracing::warn!(
            "lan.plain_admin is on and router.api_allow is empty: anyone who can reach port 80 can talk to the API; \
             set router.api_allow to your LAN, for example [\"192.168.1.0/24\"]"
        );
    }
    start_backend(paths, cfg)
}

#[cfg(feature = "lan")]
fn start_backend(paths: &Paths, cfg: &ServerConfig) -> Result<LanHandle> {
    let backend = mdns::MdnsAnnouncer::new(&cfg.lan.interfaces)?;
    let (tx, rx) = mpsc::channel();
    let mut rec = announce::Reconciler::new(backend, paths.clone(), cfg.clone());
    std::thread::Builder::new().name("mypaas-lan".into()).spawn(move || rec.run(rx))?;
    tracing::info!("LAN mode on: announcing .local names over mDNS");
    Ok(LanHandle { tx: Some(tx) })
}

#[cfg(not(feature = "lan"))]
fn start_backend(_paths: &Paths, _cfg: &ServerConfig) -> Result<LanHandle> {
    // validate_config already refused lan.enabled in this build.
    Ok(LanHandle::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lan_cfg(domain: &str, enabled: bool, plain: bool) -> ServerConfig {
        let mut cfg = ServerConfig { domain: domain.into(), ..ServerConfig::default() };
        cfg.lan.enabled = enabled;
        cfg.lan.plain_admin = plain;
        cfg
    }

    #[test]
    fn plain_admin_needs_lan_and_a_local_domain() {
        assert!(validate_config(&lan_cfg("paas.local", false, true)).is_err());
        if COMPILED {
            assert!(validate_config(&lan_cfg("paas.local", true, true)).is_ok());
            assert!(validate_config(&lan_cfg("paas.example.com", true, true)).is_err());
        }
        assert!(validate_config(&lan_cfg("paas.example.com", false, false)).is_ok());
    }

    #[test]
    fn enabling_lan_without_the_feature_fails_loudly() {
        let r = validate_config(&lan_cfg("paas.example.com", true, false));
        assert_eq!(r.is_ok(), COMPILED);
    }

    #[test]
    fn bad_lan_entries_are_refused() {
        let mut cfg = lan_cfg("paas.example.com", false, false);
        cfg.lan.allow = vec!["*.example.com".into()];
        assert!(validate_config(&cfg).is_err());
        cfg.lan.allow = vec!["shop-*.local".into()];
        assert!(validate_config(&cfg).is_ok());
        cfg.lan.interfaces = vec!["eth0; rm".into()];
        assert!(validate_config(&cfg).is_err());
    }

    #[test]
    fn plain_remotes_only_for_local_names() {
        assert_eq!(plain_remote_allowed("paas.local"), COMPILED);
        assert!(!plain_remote_allowed("paas.example.com"));
        assert!(!plain_remote_allowed("local"));
    }

    #[test]
    fn disabled_lan_starts_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { root: dir.path().to_path_buf() };
        let h = start(&paths, &ServerConfig::default()).unwrap();
        assert!(h.tx.is_none());
        h.poke();
        assert!(!state_file(&paths).exists());
    }

    #[test]
    fn state_roundtrips() {
        let mut s = LanState::default();
        s.hosts.insert("a.local".into(), HostState::Announced);
        s.hosts.insert("b.local".into(), HostState::Conflict);
        s.hosts.insert("c.local".into(), HostState::Error("x".into()));
        let back: LanState = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(s, back);
    }
}

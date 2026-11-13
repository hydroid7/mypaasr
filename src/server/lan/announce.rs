//! The reconcile loop: derive the wanted names from disk, hand them to the
//! backend, write `lan/state.json`. It runs on its own thread, so neither a
//! slow disk nor a bug in the mDNS library can stall the API.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use anyhow::Result;

use super::{HostState, LanState, hosts, state_file};
use crate::config::{Paths, ServerConfig, write_atomic};

/// How often the disk is re-read when nobody pokes. Also bounds the delay
/// after a `mypaas server deploy` in another process.
const POLL: Duration = Duration::from_secs(5);

/// An mDNS backend. Addresses are the backend's business: it follows the
/// server's interfaces itself (the `mdns-sd` backend re-checks them every 30 s).
pub trait Announcer: Send {
    /// Makes the announced set equal to `hosts`: registers new names and
    /// withdraws removed ones (with goodbye packets).
    fn set_hosts(&mut self, hosts: &BTreeSet<String>) -> Result<()>;
    /// The state of every name currently wanted.
    fn status(&mut self) -> BTreeMap<String, HostState>;
}

pub struct Reconciler<A: Announcer> {
    announcer: A,
    paths: Paths,
    cfg: ServerConfig,
    last: Option<LanState>,
}

impl<A: Announcer> Reconciler<A> {
    pub fn new(announcer: A, paths: Paths, cfg: ServerConfig) -> Self {
        Reconciler { announcer, paths, cfg, last: None }
    }

    /// Loops until every `LanHandle` is gone, waking on a poke or every `POLL`.
    pub fn run(&mut self, pokes: Receiver<()>) {
        loop {
            if let Err(e) = self.tick() {
                tracing::error!("LAN announcer: {e:#}");
            }
            match pokes.recv_timeout(POLL) {
                Ok(()) => while pokes.try_recv().is_ok() {},
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    pub fn tick(&mut self) -> Result<()> {
        let want = hosts::desired_hosts(&self.paths, &self.cfg);
        let result = self.announcer.set_hosts(&want);
        let mut hosts = self.announcer.status();
        if let Err(e) = &result {
            for h in &want {
                hosts.entry(h.clone()).or_insert_with(|| HostState::Error(format!("{e:#}")));
            }
        }
        hosts.retain(|h, _| want.contains(h));
        let changed = self.last.as_ref().is_none_or(|l| l.hosts != hosts);
        if changed {
            for (h, s) in &hosts {
                if *s == HostState::Conflict {
                    tracing::error!("LAN name {h} is already used by another device; it is not announced");
                }
            }
            let state = LanState { hosts, updated: crate::auth::unix_now() };
            let path = state_file(&self.paths);
            std::fs::create_dir_all(path.parent().expect("state file has a parent"))?;
            write_atomic(&path, &serde_json::to_vec_pretty(&state)?, 0o644)?;
            self.last = Some(state);
        }
        result
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::server::deploy::DeployRecord;
    use crate::spec::ServiceUrl;
    use std::sync::{Arc, Mutex};

    /// Records what it was asked to announce; names in `taken` conflict.
    #[derive(Clone, Default)]
    pub struct Fake {
        pub announced: Arc<Mutex<BTreeSet<String>>>,
        pub taken: Arc<Mutex<BTreeSet<String>>>,
        pub calls: Arc<Mutex<usize>>,
    }

    impl Announcer for Fake {
        fn set_hosts(&mut self, hosts: &BTreeSet<String>) -> Result<()> {
            *self.calls.lock().unwrap() += 1;
            let taken = self.taken.lock().unwrap();
            *self.announced.lock().unwrap() = hosts.iter().filter(|h| !taken.contains(*h)).cloned().collect();
            Ok(())
        }
        fn status(&mut self) -> BTreeMap<String, HostState> {
            let announced = self.announced.lock().unwrap();
            let taken = self.taken.lock().unwrap();
            let mut m: BTreeMap<_, _> = announced.iter().map(|h| (h.clone(), HostState::Announced)).collect();
            for t in taken.iter() {
                m.insert(t.clone(), HostState::Conflict);
            }
            m
        }
    }

    fn deploy(paths: &Paths, service: &str, hosts: &[&str]) {
        let df = format!("FROM x\n# mypaas.service={service}\n# mypaas.port=80\n");
        let mut spec = crate::spec::parse_spec(&df).unwrap();
        spec.urls = hosts.iter().map(|h| ServiceUrl { https: false, host: (*h).into(), path: String::new() }).collect();
        let dir = paths.service_dir(service);
        std::fs::create_dir_all(&dir).unwrap();
        let rec = DeployRecord { spec, deploy_id: "d".into(), image_id: "i".into(), time: 0, key_name: "k".into() };
        std::fs::write(dir.join("current.json"), serde_json::to_vec(&rec).unwrap()).unwrap();
    }

    fn setup() -> (tempfile::TempDir, Paths, Fake, Reconciler<Fake>) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { root: dir.path().to_path_buf() };
        let mut cfg = ServerConfig { domain: "paas.local".into(), ..ServerConfig::default() };
        cfg.lan.enabled = true;
        let fake = Fake::default();
        let rec = Reconciler::new(fake.clone(), paths.clone(), cfg);
        (dir, paths, fake, rec)
    }

    fn set(v: &[&str]) -> BTreeSet<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn reconcile_follows_deploys_renames_and_removals() {
        let (_d, paths, fake, mut rec) = setup();
        rec.tick().unwrap();
        assert_eq!(*fake.announced.lock().unwrap(), set(&["paas.local"]));
        deploy(&paths, "shop", &["shop.local"]);
        rec.tick().unwrap();
        assert_eq!(*fake.announced.lock().unwrap(), set(&["paas.local", "shop.local"]));
        deploy(&paths, "shop", &["store.local"]);
        rec.tick().unwrap();
        assert_eq!(*fake.announced.lock().unwrap(), set(&["paas.local", "store.local"]));
        std::fs::remove_dir_all(paths.service_dir("shop")).unwrap();
        rec.tick().unwrap();
        assert_eq!(*fake.announced.lock().unwrap(), set(&["paas.local"]));
        let state = super::super::read_state(&paths).unwrap();
        assert_eq!(state.hosts.keys().cloned().collect::<BTreeSet<_>>(), set(&["paas.local"]));
    }

    #[test]
    fn conflicts_reach_the_state_file() {
        let (_d, paths, fake, mut rec) = setup();
        fake.taken.lock().unwrap().insert("shop.local".into());
        deploy(&paths, "shop", &["shop.local"]);
        rec.tick().unwrap();
        let state = super::super::read_state(&paths).unwrap();
        assert_eq!(state.hosts["shop.local"], HostState::Conflict);
        assert_eq!(state.hosts["paas.local"], HostState::Announced);
    }

    #[test]
    fn a_poke_wakes_the_loop_and_dropping_the_handle_stops_it() {
        let (_d, paths, fake, mut rec) = setup();
        let (tx, rx) = std::sync::mpsc::channel();
        let t = std::thread::spawn(move || rec.run(rx));
        deploy(&paths, "shop", &["shop.local"]);
        tx.send(()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !fake.announced.lock().unwrap().contains("shop.local") {
            assert!(std::time::Instant::now() < deadline, "poke did not trigger a reconcile");
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(tx);
        t.join().unwrap();
    }
}

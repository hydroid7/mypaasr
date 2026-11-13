//! The `mdns-sd` backend: each name is registered as an `_http._tcp` service
//! whose host is that name, which makes the library answer A queries for it.
//! The library runs its own thread and probes before announcing (RFC 6762).

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use mdns_sd::{DaemonEvent, IfKind, IfPredicate, ServiceDaemon, ServiceInfo};

use super::HostState;
use super::announce::Announcer;

const SERVICE_TYPE: &str = "_http._tcp.local.";

pub struct MdnsAnnouncer {
    daemon: ServiceDaemon,
    events: mdns_sd::Receiver<DaemonEvent>,
    /// host -> the service fullname registered for it.
    registered: BTreeMap<String, String>,
    conflicts: BTreeSet<String>,
}

/// Docker's bridges and veth pairs only reach containers.
fn is_docker_interface(name: &str) -> bool {
    name == "docker0" || name.starts_with("br-") || name.starts_with("veth") || name.starts_with("docker")
}

impl MdnsAnnouncer {
    pub fn new(interfaces: &[String]) -> Result<Self> {
        let daemon = ServiceDaemon::new().context("cannot start the mDNS responder")?;
        // IPv6 stays on, so names get AAAA records as well:
        // resolvers ask for A and AAAA together, and with no responder on IPv6
        // the AAAA query only times out, delaying every lookup by about 5 s
        // (spike, nss-mdns with Avahi). Loopback and Docker interfaces never.
        daemon.disable_interface(vec![IfKind::LoopbackV4, IfKind::LoopbackV6])?;
        if interfaces.is_empty() {
            daemon.disable_interface(IfKind::Predicate(IfPredicate::new(|i| is_docker_interface(&i.name))))?;
        } else {
            let wanted = interfaces.to_vec();
            daemon.disable_interface(IfKind::Predicate(IfPredicate::new(move |i| !wanted.contains(&i.name))))?;
        }
        // Follow DHCP changes; the A records are rewritten from the new addresses.
        daemon.set_ip_check_interval(30)?;
        let events = daemon.monitor()?;
        Ok(MdnsAnnouncer { daemon, events, registered: BTreeMap::new(), conflicts: BTreeSet::new() })
    }

    /// A name the library renamed on conflict (`shop.local.` → `shop-2.local.`)
    /// is useless, since Traefik routes by the exact Host: withdraw it.
    fn drain_events(&mut self) {
        while let Ok(ev) = self.events.try_recv() {
            match ev {
                DaemonEvent::NameChange(c) => {
                    let original = c.original.trim_end_matches('.').to_string();
                    let host = if let Some((h, _)) = self.registered.iter().find(|(h, full)| **h == original || **full == c.original) {
                        h.clone()
                    } else {
                        continue;
                    };
                    tracing::warn!("mDNS conflict on {}: another device uses {host}", c.intf_name);
                    if let Some(full) = self.registered.remove(&host) {
                        let _ = self.daemon.unregister(&full);
                    }
                    self.conflicts.insert(host);
                }
                DaemonEvent::Error(e) => tracing::warn!("mDNS: {e}"),
                _ => {}
            }
        }
    }
}

impl Announcer for MdnsAnnouncer {
    fn set_hosts(&mut self, hosts: &BTreeSet<String>) -> Result<()> {
        self.drain_events();
        // A conflict is kept until the name is no longer wanted, so it does not
        // flap between probing and losing every few seconds.
        self.conflicts.retain(|h| hosts.contains(h));
        let gone: Vec<String> = self.registered.keys().filter(|h| !hosts.contains(*h)).cloned().collect();
        for h in gone {
            if let Some(full) = self.registered.remove(&h) {
                let _ = self.daemon.unregister(&full);
            }
        }
        let mut first_err = None;
        for h in hosts {
            if self.registered.contains_key(h) || self.conflicts.contains(h) {
                continue;
            }
            let instance = h.trim_end_matches(".local");
            let info = ServiceInfo::new(SERVICE_TYPE, instance, &format!("{h}."), "", 80, None::<std::collections::HashMap<String, String>>)
                .map(ServiceInfo::enable_addr_auto)
                .with_context(|| format!("cannot describe {h}"));
            let r = info.and_then(|info| {
                let full = info.get_fullname().to_string();
                self.daemon.register(info).with_context(|| format!("cannot announce {h}"))?;
                Ok(full)
            });
            match r {
                Ok(full) => {
                    self.registered.insert(h.clone(), full);
                }
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            }
        }
        first_err.map_or(Ok(()), Err)
    }

    fn status(&mut self) -> BTreeMap<String, HostState> {
        self.drain_events();
        let mut m: BTreeMap<String, HostState> = self.registered.keys().map(|h| (h.clone(), HostState::Announced)).collect();
        for c in &self.conflicts {
            m.insert(c.clone(), HostState::Conflict);
        }
        m
    }
}

impl Drop for MdnsAnnouncer {
    fn drop(&mut self) {
        // Sends goodbye packets for every name. Waiting matters: without it the
        // process can exit first and the names linger in caches for their TTL.
        if let Ok(rx) = self.daemon.shutdown() {
            let _ = rx.recv_timeout(std::time::Duration::from_secs(2));
        }
    }
}

//! Which `.local` names to announce, and which a deploy may claim. Pure apart
//! from reading `current.json` files, so it needs neither the feature nor a socket.

use std::collections::BTreeSet;

use anyhow::{Result, bail};

use crate::config::{Paths, ServerConfig};
use crate::server::deploy::read_record;
use crate::spec::ServiceSpec;

/// mDNS resolves only names under `.local`; nothing else is announced.
pub fn is_lan_host(host: &str) -> bool {
    host.strip_suffix(".local").is_some_and(|rest| !rest.is_empty() && !rest.ends_with('.'))
}

/// This machine's own mDNS name (`<hostname>.local`), which Avahi or
/// systemd-resolved may already announce. No service may take it.
fn server_name() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most buf.len() bytes into buf.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let name = std::str::from_utf8(&buf[..end]).ok()?.split('.').next()?.to_ascii_lowercase();
    (!name.is_empty()).then(|| format!("{name}.local"))
}

/// Checks a spec's URLs at deploy time, beside the admin-domain check. Returns
/// warnings to show the deployer. Does nothing unless `lan.enabled`.
pub fn check_urls(spec: &ServiceSpec, cfg: &ServerConfig) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    if !cfg.lan.enabled {
        return Ok(warnings);
    }
    let server = server_name();
    for u in spec.urls.iter().filter(|u| is_lan_host(&u.host)) {
        if u.https {
            // Traefik would ask Let's Encrypt for a certificate that can never be
            // issued, and sniStrict would then refuse every handshake.
            bail!("url {}: HTTPS for .local needs a certificate, which LAN mode does not provide yet; use http://", u.to_url());
        }
        if server.as_deref() == Some(u.host.as_str()) {
            bail!("url {} is this server's own mDNS name, which is reserved", u.to_url());
        }
        if !cfg.lan.allow.iter().any(|p| glob_match::glob_match(p, &u.host)) {
            bail!("url {} is not allowed by lan.allow in the server's config.toml", u.to_url());
        }
        if u.host.matches('.').count() > 1 {
            warnings.push(format!(
                "warning: {} has more than one label before .local; some resolvers (nss-mdns in its default mode) only look up names like shop.local",
                u.host
            ));
        }
    }
    Ok(warnings)
}

/// Every `.local` host in a deployed `current.json`, plus the admin domain
/// when it is a `.local` name. Read from disk, so deploys made by
/// `mypaas server deploy` in another process are seen too.
#[cfg_attr(not(feature = "lan"), allow(dead_code))]
pub fn desired_hosts(paths: &Paths, cfg: &ServerConfig) -> BTreeSet<String> {
    let mut hosts = BTreeSet::new();
    if is_lan_host(&cfg.domain) {
        hosts.insert(cfg.domain.clone());
    }
    let Ok(entries) = std::fs::read_dir(paths.services_dir()) else { return hosts };
    for entry in entries.flatten() {
        let Ok(Some(rec)) = read_record(&entry.path().join("current.json")) else { continue };
        for u in &rec.spec.urls {
            // Deploy refuses these, but a record written before LAN mode was
            // switched on was never checked.
            if is_lan_host(&u.host) && !u.https && u.host != cfg.domain {
                hosts.insert(u.host.clone());
            }
        }
    }
    hosts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::deploy::DeployRecord;
    use crate::spec::ServiceUrl;

    fn url(s: &str) -> ServiceUrl {
        let (scheme, host) = s.split_once("://").unwrap();
        ServiceUrl { https: scheme == "https", host: host.into(), path: String::new() }
    }

    fn spec(name: &str, urls: &[&str]) -> ServiceSpec {
        let df = format!("FROM x\n# mypaas.service={name}\n# mypaas.port=80\n");
        let mut spec = crate::spec::parse_spec(&df).unwrap();
        spec.urls = urls.iter().map(|u| url(u)).collect();
        spec
    }

    fn enabled() -> ServerConfig {
        let mut cfg = ServerConfig { domain: "paas.example.com".into(), ..ServerConfig::default() };
        cfg.lan.enabled = true;
        cfg
    }

    fn write(paths: &Paths, s: ServiceSpec) {
        let dir = paths.service_dir(&s.service);
        std::fs::create_dir_all(&dir).unwrap();
        let rec = DeployRecord { spec: s, deploy_id: "d".into(), image_id: "i".into(), time: 0, key_name: "k".into() };
        std::fs::write(dir.join("current.json"), serde_json::to_vec(&rec).unwrap()).unwrap();
    }

    #[test]
    fn only_local_names_are_lan_hosts() {
        assert!(is_lan_host("shop.local"));
        assert!(is_lan_host("api.shop.local"));
        assert!(!is_lan_host("local"));
        assert!(!is_lan_host(".local"));
        assert!(!is_lan_host("shop.localhost"));
        assert!(!is_lan_host("shop.example.com"));
    }

    #[test]
    fn hosts_come_from_every_deployed_spec() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { root: dir.path().to_path_buf() };
        let mut cfg = enabled();
        assert!(desired_hosts(&paths, &cfg).is_empty(), "no services dir yet");
        write(&paths, spec("shop", &["https://shop.example.com", "http://shop.local"]));
        write(&paths, spec("blog", &["http://blog.local", "http://api.blog.local"]));
        write(&paths, spec("worker", &[]));
        let want: BTreeSet<String> = ["api.blog.local", "blog.local", "shop.local"].map(String::from).into();
        assert_eq!(desired_hosts(&paths, &cfg), want);
        cfg.domain = "paas.local".into();
        assert!(desired_hosts(&paths, &cfg).contains("paas.local"), "the admin domain is announced too");
    }

    #[test]
    fn https_on_local_is_refused() {
        let err = check_urls(&spec("shop", &["https://shop.local"]), &enabled()).unwrap_err();
        assert!(err.to_string().contains("certificate"));
        assert!(check_urls(&spec("shop", &["http://shop.local", "https://shop.example.com"]), &enabled()).unwrap().is_empty());
    }

    #[test]
    fn checks_are_skipped_when_lan_is_off() {
        assert!(check_urls(&spec("shop", &["https://shop.local"]), &ServerConfig::default()).is_ok());
    }

    #[test]
    fn deep_names_warn() {
        let w = check_urls(&spec("shop", &["http://api.shop.local"]), &enabled()).unwrap();
        assert_eq!(w.len(), 1);
    }

    #[test]
    fn allow_globs_limit_claims() {
        let mut cfg = enabled();
        cfg.lan.allow = vec!["shop-*.local".into()];
        assert!(check_urls(&spec("a", &["http://shop-a.local"]), &cfg).is_ok());
        assert!(check_urls(&spec("a", &["http://printer.local"]), &cfg).is_err());
    }

    #[test]
    fn server_name_is_reserved() {
        if let Some(own) = server_name() {
            assert!(check_urls(&spec("a", &[&format!("http://{own}")]), &enabled()).is_err());
        }
    }
}

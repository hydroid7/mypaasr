//! Keys, scopes and request signatures, shared by client and daemon.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};
use ssh_key::{HashAlg, PublicKey};

pub const HEADER_KEY: &str = "x-mypaas-key";
pub const HEADER_TIME: &str = "x-mypaas-time";
pub const HEADER_NONCE: &str = "x-mypaas-nonce";
pub const HEADER_SIG: &str = "x-mypaas-sig";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    Read,
    Logs,
    Deploy(String),
    Secrets(String),
    Rm(String),
    Admin,
}

impl Scope {
    pub fn parse(s: &str) -> Result<Scope> {
        let glob = |g: &str| -> Result<String> {
            let ok = !g.is_empty()
                && g.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"-_*?".contains(&c));
            if !ok {
                bail!("invalid service pattern {g:?} in scope {s:?}");
            }
            Ok(g.to_string())
        };
        Ok(match s.split_once(':') {
            None if s == "read" => Scope::Read,
            None if s == "logs" => Scope::Logs,
            None if s == "admin" => Scope::Admin,
            Some(("deploy", g)) => Scope::Deploy(glob(g)?),
            Some(("secrets", g)) => Scope::Secrets(glob(g)?),
            Some(("rm", g)) => Scope::Rm(glob(g)?),
            _ => bail!("unknown scope {s:?}; use read, logs, deploy:<glob>, secrets:<glob>, rm:<glob> or admin"),
        })
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Scope::Read => write!(f, "read"),
            Scope::Logs => write!(f, "logs"),
            Scope::Admin => write!(f, "admin"),
            Scope::Deploy(g) => write!(f, "deploy:{g}"),
            Scope::Secrets(g) => write!(f, "secrets:{g}"),
            Scope::Rm(g) => write!(f, "rm:{g}"),
        }
    }
}

/// What a request needs; checked against the key's scopes.
#[derive(Debug, Clone, Copy)]
pub enum Need<'a> {
    Read,
    Logs,
    Deploy(&'a str),
    Secrets(&'a str),
    Rm(&'a str),
    Admin,
}

pub fn allows(scopes: &[Scope], need: Need) -> bool {
    scopes.iter().any(|s| match (s, need) {
        (Scope::Admin, _) => true,
        (Scope::Read, Need::Read) | (Scope::Logs, Need::Logs) => true,
        (Scope::Deploy(g), Need::Deploy(svc))
        | (Scope::Secrets(g), Need::Secrets(svc))
        | (Scope::Rm(g), Need::Rm(svc)) => glob_match::glob_match(g, svc),
        _ => false,
    })
}

/// One line of `authorized_keys`.
#[derive(Debug, Clone)]
pub struct AuthorizedKey {
    pub name: String,
    pub key_id: String,
    pub key: VerifyingKey,
    pub public_line: String,
    pub scopes: Vec<Scope>,
    /// Unix time at which the key stops working.
    pub expires: Option<u64>,
}

impl AuthorizedKey {
    pub fn is_expired(&self, now: u64) -> bool {
        self.expires.is_some_and(|e| now >= e)
    }

    pub fn to_line(&self) -> String {
        let scopes: Vec<String> = self.scopes.iter().map(|s| s.to_string()).collect();
        let mut line = format!("{} {} scope={}", self.public_line, self.name, scopes.join(","));
        if let Some(e) = self.expires {
            line.push_str(&format!(" expires={}", format_date(e)));
        }
        line
    }
}

pub fn is_valid_key_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes().all(|c| c.is_ascii_alphanumeric() || b"-_.@".contains(&c))
}

/// Parses `ssh-ed25519 AAAA… name scope=a,b expires=YYYY-MM-DD`.
pub fn parse_authorized_line(line: &str) -> Result<AuthorizedKey> {
    let words: Vec<&str> = line.split_whitespace().collect();
    if words.len() < 3 {
        bail!("expected: ssh-ed25519 <key> <name> scope=<scopes> [expires=YYYY-MM-DD]");
    }
    let (key, public_line, key_id) = parse_public_key(&format!("{} {}", words[0], words[1]))?;
    let name = words[2];
    if !is_valid_key_name(name) || name.contains('=') {
        bail!("invalid key name {name:?}");
    }
    let mut scopes = Vec::new();
    let mut expires = None;
    for w in &words[3..] {
        if let Some(list) = w.strip_prefix("scope=") {
            for s in list.split(',').filter(|s| !s.is_empty()) {
                scopes.push(Scope::parse(s)?);
            }
        } else if let Some(d) = w.strip_prefix("expires=") {
            expires = Some(parse_date(d)?);
        } else {
            bail!("unknown option {w:?}");
        }
    }
    if scopes.is_empty() {
        bail!("key {name} has no scope");
    }
    Ok(AuthorizedKey { name: name.to_string(), key_id, key, public_line, scopes, expires })
}

/// Parses all lines, skipping blanks and `#` comments. Bad lines are errors
/// so a typo is noticed rather than silently dropping a key.
pub fn parse_authorized_keys(text: &str) -> Result<Vec<AuthorizedKey>> {
    let mut keys = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        keys.push(parse_authorized_line(line).with_context(|| format!("authorized_keys line {}", i + 1))?);
    }
    Ok(keys)
}

/// Returns the verifying key, the `ssh-ed25519 AAAA…` text and the key id.
pub fn parse_public_key(text: &str) -> Result<(VerifyingKey, String, String)> {
    let pk = PublicKey::from_openssh(text.trim()).context("not an OpenSSH public key")?;
    let ed = pk.key_data().ed25519().context("only ssh-ed25519 keys are supported")?;
    let key = VerifyingKey::from_bytes(&ed.0).context("invalid Ed25519 public key")?;
    let mut bare = pk.clone();
    bare.set_comment("");
    let line = bare.to_openssh()?.trim().to_string();
    Ok((key, line, key_id(&pk)))
}

/// SHA-256 fingerprint as printed by `ssh-keygen -l`, e.g. `SHA256:abc…`.
pub fn key_id(pk: &PublicKey) -> String {
    pk.fingerprint(HashAlg::Sha256).to_string()
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// The string that is signed for every small request. The host binds the
/// signature to one server: a request signed for server B cannot be relayed
/// to server A, even when both trust the same key.
pub fn canonical_string(method: &str, host: &str, path_and_query: &str, time: u64, nonce: &str, body: &[u8]) -> String {
    format!(
        "mypaas/v1\n{}\n{method}\n{path_and_query}\n{time}\n{nonce}\n{}",
        host.to_ascii_lowercase(),
        sha256_hex(body)
    )
}

pub fn sign(key: &SigningKey, canonical: &str) -> String {
    B64.encode(key.sign(canonical.as_bytes()).to_bytes())
}

pub fn verify(key: &VerifyingKey, canonical: &str, sig_b64: &str) -> bool {
    let Ok(bytes) = B64.decode(sig_b64) else { return false };
    let Ok(sig) = Signature::from_slice(&bytes) else { return false };
    key.verify(canonical.as_bytes(), &sig).is_ok()
}

pub fn is_valid_nonce(n: &str) -> bool {
    (16..=64).contains(&n.len()) && n.bytes().all(|c| c.is_ascii_hexdigit())
}

pub fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// `YYYY-MM-DD` to the unix time of 00:00 UTC that day.
pub fn parse_date(s: &str) -> Result<u64> {
    let parts: Vec<&str> = s.split('-').collect();
    let bad = || format!("invalid date {s:?}, expected YYYY-MM-DD");
    let [y, m, d] = parts[..] else { bail!(bad()) };
    let (y, m, d): (i64, u32, u32) = (
        y.parse().with_context(bad)?,
        m.parse().with_context(bad)?,
        d.parse().with_context(bad)?,
    );
    if !(1970..=9999).contains(&y) || !(1..=12).contains(&m) || d < 1 || d > days_in_month(y, m) {
        bail!(bad());
    }
    Ok(days_from_civil(y, m, d) as u64 * 86_400)
}

pub fn format_date(unix: u64) -> String {
    let (y, m, d) = civil_from_days((unix / 86_400) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `2026-11-07T11:21:12.675180123Z` (UTC only, fraction optional) to unix seconds.
pub fn parse_rfc3339_utc(s: &str) -> Option<u64> {
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let day = parse_date(date).ok()?;
    let time = time.split('.').next()?;
    let mut parts = time.split(':').map(|p| p.parse::<u64>().ok());
    let (h, m, sec) = (parts.next()??, parts.next()??, parts.next()??);
    if parts.next().is_some() || h > 23 || m > 59 || sec > 60 {
        return None;
    }
    Some(day + h * 3600 + m * 60 + sec)
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

// Howard Hinnant's algorithms for the proleptic Gregorian calendar.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ssh_key::{Algorithm, PrivateKey};

    pub fn test_key() -> (SigningKey, String) {
        let pk = PrivateKey::random(&mut rand::rngs::OsRng, Algorithm::Ed25519).unwrap();
        let ed = pk.key_data().ed25519().unwrap();
        let signing = SigningKey::from_bytes(&ed.private.to_bytes());
        (signing, pk.public_key().to_openssh().unwrap())
    }

    #[test]
    fn scopes() {
        let s = vec![Scope::parse("deploy:shop-*").unwrap(), Scope::parse("read").unwrap()];
        assert!(allows(&s, Need::Deploy("shop-web")));
        assert!(!allows(&s, Need::Deploy("blog")));
        assert!(!allows(&s, Need::Rm("shop-web")));
        assert!(allows(&s, Need::Read));
        assert!(!allows(&s, Need::Admin));
        assert!(allows(&[Scope::Admin], Need::Secrets("x")));
        assert!(Scope::parse("deploy:../x").is_err());
        assert!(Scope::parse("root").is_err());
    }

    #[test]
    fn authorized_line_roundtrip() {
        let (_, public) = test_key();
        let line = format!("{public} ci scope=deploy:shop-*,read expires=2027-01-01");
        let k = parse_authorized_line(&line).unwrap();
        assert_eq!(k.name, "ci");
        assert_eq!(k.scopes.len(), 2);
        assert!(k.key_id.starts_with("SHA256:"));
        assert!(!k.is_expired(parse_date("2026-12-31").unwrap()));
        assert!(k.is_expired(parse_date("2027-01-01").unwrap()));
        let again = parse_authorized_line(&k.to_line()).unwrap();
        assert_eq!(again.key_id, k.key_id);
        assert_eq!(again.expires, k.expires);
        assert!(parse_authorized_line(&format!("{public} ci")).is_err());
        assert!(parse_authorized_line(&format!("{public} ci scope=read color=red")).is_err());
    }

    #[test]
    fn sign_and_verify() {
        let (sk, public) = test_key();
        let (vk, _, _) = parse_public_key(&public).unwrap();
        let c = canonical_string("POST", "a.example.com", "/v1/deploy-tickets", 1000, "00ff00ff00ff00ff", b"{}");
        let sig = sign(&sk, &c);
        assert!(verify(&vk, &c, &sig));
        assert!(verify(&vk, &canonical_string("POST", "A.Example.com", "/v1/deploy-tickets", 1000, "00ff00ff00ff00ff", b"{}"), &sig));
        let other = canonical_string("POST", "a.example.com", "/v1/deploy-tickets", 1000, "00ff00ff00ff00ff", b"{ }");
        assert!(!verify(&vk, &other, &sig));
        let elsewhere = canonical_string("POST", "b.example.com", "/v1/deploy-tickets", 1000, "00ff00ff00ff00ff", b"{}");
        assert!(!verify(&vk, &elsewhere, &sig), "a signature is valid for one server only");
        assert!(!verify(&vk, &c, "garbage"));
    }

    #[test]
    fn dates() {
        assert_eq!(parse_date("1970-01-01").unwrap(), 0);
        assert_eq!(parse_date("2000-03-01").unwrap(), 951_868_800);
        assert_eq!(format_date(parse_date("2024-02-29").unwrap()), "2024-02-29");
        assert!(parse_date("2023-02-29").is_err());
        assert!(parse_date("2023-13-01").is_err());
        assert_eq!(parse_rfc3339_utc("1970-01-01T00:01:05.123456789Z"), Some(65));
        assert_eq!(parse_rfc3339_utc("2000-03-01T00:00:00Z"), Some(951_868_800));
        assert_eq!(parse_rfc3339_utc("2000-03-01T00:00:00+02:00"), None);
        assert_eq!(parse_rfc3339_utc("2000-03-01T24:00:00Z"), None);
        assert_eq!(parse_rfc3339_utc("garbage"), None);
    }
}

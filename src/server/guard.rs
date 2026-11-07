//! In-memory abuse protection: failure backoff, nonces, tickets and rate limits.
//!
//! All tables are bounded so a flood of requests cannot grow memory.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use rand::RngCore;
use sha2::{Digest, Sha256};

pub const FREE_FAILURES: u32 = 3;
pub const MAX_LOCKOUT: Duration = Duration::from_secs(3600);
pub const FAILURE_MEMORY: Duration = Duration::from_secs(24 * 3600);
pub const MAX_IPS: usize = 10_000;

pub const NONCE_TTL: Duration = Duration::from_secs(60);
pub const MAX_NONCES: usize = 10_000;

pub const TICKET_TTL: Duration = Duration::from_secs(60);
pub const MAX_TICKETS: usize = 4;
pub const MAX_TICKETS_PER_KEY: usize = 2;

pub const SIG_CHECKS_PER_SEC: u32 = 50;

/// IPv4 addresses are tracked exactly, IPv6 per /64.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IpKey(u128);

impl From<IpAddr> for IpKey {
    fn from(ip: IpAddr) -> Self {
        match ip {
            IpAddr::V4(v4) => IpKey(u32::from(v4) as u128),
            IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
                Some(v4) => IpKey(u32::from(v4) as u128),
                None => IpKey((u128::from(v6) & !((1u128 << 64) - 1)) | (1 << 127) | 1),
            },
        }
    }
}

struct FailEntry {
    failures: u32,
    locked_until: Instant,
    last_failure: Instant,
}

#[derive(Default)]
pub struct Backoff {
    table: HashMap<IpKey, FailEntry>,
}

impl Backoff {
    /// Returns the remaining lockout, if any.
    pub fn locked(&mut self, ip: IpKey, now: Instant) -> Option<Duration> {
        let e = self.table.get(&ip)?;
        if now.duration_since(e.last_failure) > FAILURE_MEMORY {
            self.table.remove(&ip);
            return None;
        }
        (e.locked_until > now).then(|| e.locked_until - now)
    }

    pub fn fail(&mut self, ip: IpKey, now: Instant) {
        if !self.table.contains_key(&ip) && self.table.len() >= MAX_IPS {
            self.evict(now);
        }
        let e = self.table.entry(ip).or_insert(FailEntry { failures: 0, locked_until: now, last_failure: now });
        if now.duration_since(e.last_failure) > FAILURE_MEMORY {
            e.failures = 0;
        }
        e.failures = e.failures.saturating_add(1);
        e.last_failure = now;
        if e.failures > FREE_FAILURES {
            e.locked_until = now + lockout_for(e.failures);
        }
    }

    pub fn success(&mut self, ip: IpKey) {
        self.table.remove(&ip);
    }

    /// Drops expired entries, or else the least recently failed one.
    fn evict(&mut self, now: Instant) {
        self.table.retain(|_, e| now.duration_since(e.last_failure) <= FAILURE_MEMORY);
        if self.table.len() >= MAX_IPS
            && let Some(oldest) = self.table.iter().min_by_key(|(_, e)| e.last_failure).map(|(k, _)| *k)
        {
            self.table.remove(&oldest);
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.table.len()
    }
}

/// Lockout after the n-th failure: 2^(n-3) seconds, capped at an hour.
pub fn lockout_for(failures: u32) -> Duration {
    if failures <= FREE_FAILURES {
        return Duration::ZERO;
    }
    let exp = failures - FREE_FAILURES;
    if exp >= 12 {
        return MAX_LOCKOUT;
    }
    Duration::from_secs(1 << exp).min(MAX_LOCKOUT)
}

#[derive(Default)]
pub struct Nonces {
    seen: HashMap<String, Instant>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum NonceResult {
    Fresh,
    Replayed,
    Full,
}

impl Nonces {
    pub fn check_and_insert(&mut self, key_id: &str, nonce: &str, now: Instant) -> NonceResult {
        let k = format!("{key_id} {nonce}");
        if let Some(t) = self.seen.get(&k)
            && now.duration_since(*t) < NONCE_TTL
        {
            return NonceResult::Replayed;
        }
        if self.seen.len() >= MAX_NONCES {
            self.seen.retain(|_, t| now.duration_since(*t) < NONCE_TTL);
            if self.seen.len() >= MAX_NONCES {
                return NonceResult::Full;
            }
        }
        self.seen.insert(k, now);
        NonceResult::Fresh
    }
}

#[derive(Debug, Clone)]
pub struct Ticket {
    pub key_id: String,
    pub key_name: String,
    pub admin: bool,
    pub service: String,
    pub size: u64,
    pub sha256: String,
    pub expires: Instant,
}

#[derive(Default)]
pub struct Tickets {
    /// Keyed by the SHA-256 of the token, so the table never holds a usable token.
    open: HashMap<[u8; 32], Ticket>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum IssueError {
    TooManyOpen,
    TooManyForKey,
}

fn token_hash(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

impl Tickets {
    fn purge(&mut self, now: Instant) {
        self.open.retain(|_, t| t.expires > now);
    }

    pub fn issue(&mut self, ticket: Ticket, now: Instant) -> Result<String, IssueError> {
        self.purge(now);
        if self.open.len() >= MAX_TICKETS {
            return Err(IssueError::TooManyOpen);
        }
        if self.open.values().filter(|t| t.key_id == ticket.key_id).count() >= MAX_TICKETS_PER_KEY {
            return Err(IssueError::TooManyForKey);
        }
        let mut raw = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut raw);
        let token = hex::encode(raw);
        self.open.insert(token_hash(&token), ticket);
        Ok(token)
    }

    pub fn peek(&mut self, token: &str, now: Instant) -> Option<Ticket> {
        self.purge(now);
        self.open.get(&token_hash(token)).cloned()
    }

    pub fn consume(&mut self, token: &str, now: Instant) -> Option<Ticket> {
        self.purge(now);
        self.open.remove(&token_hash(token))
    }
}

/// A token bucket for the global cap on signature checks.
pub struct RateLimiter {
    per_sec: u32,
    tokens: f64,
    last: Instant,
}

impl RateLimiter {
    pub fn new(per_sec: u32, now: Instant) -> Self {
        RateLimiter { per_sec, tokens: per_sec as f64, last: now }
    }

    pub fn try_take(&mut self, now: Instant) -> bool {
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * self.per_sec as f64).min(self.per_sec as f64);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpKey {
        s.parse::<IpAddr>().unwrap().into()
    }

    #[test]
    fn ipv6_grouped_per_64() {
        assert_eq!(ip("2001:db8:1:2::1"), ip("2001:db8:1:2:ffff::9"));
        assert_ne!(ip("2001:db8:1:2::1"), ip("2001:db8:1:3::1"));
        assert_eq!(ip("::ffff:1.2.3.4"), ip("1.2.3.4"));
        assert_ne!(ip("1.2.3.4"), ip("1.2.3.5"));
    }

    #[test]
    fn lockout_schedule() {
        assert_eq!(lockout_for(3), Duration::ZERO);
        assert_eq!(lockout_for(4), Duration::from_secs(2));
        assert_eq!(lockout_for(5), Duration::from_secs(4));
        assert_eq!(lockout_for(15), MAX_LOCKOUT);
        assert_eq!(lockout_for(u32::MAX), MAX_LOCKOUT);
    }

    #[test]
    fn backoff_locks_and_clears() {
        let mut b = Backoff::default();
        let t = Instant::now();
        let a = ip("10.0.0.1");
        for _ in 0..3 {
            b.fail(a, t);
        }
        assert_eq!(b.locked(a, t), None);
        b.fail(a, t);
        assert_eq!(b.locked(a, t), Some(Duration::from_secs(2)));
        assert_eq!(b.locked(a, t + Duration::from_secs(3)), None);
        b.success(a);
        b.fail(a, t);
        assert_eq!(b.locked(a, t), None);
        // Forgotten after a day without failures.
        for _ in 0..10 {
            b.fail(a, t);
        }
        assert!(b.locked(a, t + Duration::from_secs(100)).is_some());
        assert_eq!(b.locked(a, t + FAILURE_MEMORY + Duration::from_secs(1)), None);
    }

    #[test]
    fn backoff_is_bounded() {
        let mut b = Backoff::default();
        let t = Instant::now();
        for i in 0..(MAX_IPS as u32 + 50) {
            b.fail(IpKey(i as u128), t + Duration::from_millis(i as u64));
        }
        assert_eq!(b.len(), MAX_IPS);
        // The newest entries survive.
        assert!(b.table.contains_key(&IpKey(MAX_IPS as u128 + 49)));
        assert!(!b.table.contains_key(&IpKey(0)));
    }

    #[test]
    fn nonces() {
        let mut n = Nonces::default();
        let t = Instant::now();
        assert_eq!(n.check_and_insert("k", "a", t), NonceResult::Fresh);
        assert_eq!(n.check_and_insert("k", "a", t), NonceResult::Replayed);
        assert_eq!(n.check_and_insert("k2", "a", t), NonceResult::Fresh);
        assert_eq!(n.check_and_insert("k", "a", t + NONCE_TTL), NonceResult::Fresh);
    }

    fn ticket(key: &str, expires: Instant) -> Ticket {
        Ticket {
            key_id: key.into(),
            key_name: "n".into(),
            admin: false,
            service: "s".into(),
            size: 1,
            sha256: "h".into(),
            expires,
        }
    }

    #[test]
    fn tickets_single_use_and_capped() {
        let mut ts = Tickets::default();
        let t = Instant::now();
        let exp = t + TICKET_TTL;
        let a = ts.issue(ticket("k1", exp), t).unwrap();
        ts.issue(ticket("k1", exp), t).unwrap();
        assert_eq!(ts.issue(ticket("k1", exp), t), Err(IssueError::TooManyForKey));
        ts.issue(ticket("k2", exp), t).unwrap();
        ts.issue(ticket("k3", exp), t).unwrap();
        assert_eq!(ts.issue(ticket("k4", exp), t), Err(IssueError::TooManyOpen));
        assert!(ts.peek(&a, t).is_some());
        assert!(ts.consume(&a, t).is_some());
        assert!(ts.consume(&a, t).is_none());
        assert!(ts.peek("deadbeef", t).is_none());
        // Expired tickets are gone and free their slots.
        assert!(ts.issue(ticket("k4", exp + TICKET_TTL), exp).is_ok());
    }

    #[test]
    fn rate_limiter() {
        let t = Instant::now();
        let mut r = RateLimiter::new(2, t);
        assert!(r.try_take(t));
        assert!(r.try_take(t));
        assert!(!r.try_take(t));
        assert!(r.try_take(t + Duration::from_millis(600)));
    }
}

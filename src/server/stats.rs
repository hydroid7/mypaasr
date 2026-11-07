//! Per-service history: traffic from Traefik's JSON access log, and uptime and
//! resource use from samples of the containers.
//!
//! The daemon tails the log, counts requests per service in hourly buckets
//! (status classes and a latency histogram) and keeps them for the configured
//! retention. The access log only has the fields needed here; client addresses,
//! paths and headers are dropped by Traefik before they reach the disk.
//!
//! Once a minute it also records, per deployed service, whether its replicas
//! are running and how much memory and CPU they use, into hourly buckets of
//! the same retention.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::auth::{parse_rfc3339_utc, unix_now};
use crate::config::{Paths, ROUTER_CONTAINER, write_atomic};
use crate::spec::is_valid_name;

pub const ACCESS_LOG: &str = "access.log";
/// Where Traefik sees the log directory.
pub const LOG_MOUNT: &str = "/mypaas-logs";
const INGEST_INTERVAL: Duration = Duration::from_secs(10);
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(60);
/// The log is rotated once this much of it has been read.
const ROTATE_AT: u64 = 32 << 20;
const MAX_LINE: usize = 64 * 1024;
/// Latency histogram: bin i holds durations around 2^(i/4) microseconds,
/// about 19% wide, from 1µs up to about 18 minutes.
const BINS_PER_OCTAVE: f64 = 4.0;
const MAX_BIN: u8 = 120;

pub fn log_dir(paths: &Paths) -> PathBuf {
    paths.root.join("logs")
}

fn stats_dir(paths: &Paths) -> PathBuf {
    paths.root.join("stats")
}

fn latency_bin(duration_ns: u64) -> u8 {
    let us = (duration_ns / 1000).max(1) as f64;
    ((us.log2() * BINS_PER_OCTAVE).floor() as u64).min(MAX_BIN as u64) as u8
}

/// The geometric middle of a bin, in milliseconds.
fn bin_ms(bin: u8) -> f64 {
    2f64.powf((bin as f64 + 0.5) / BINS_PER_OCTAVE) / 1000.0
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Bucket {
    pub requests: u64,
    /// Requests per status class, 1xx to 5xx.
    pub status: [u64; 5],
    /// Sparse latency histogram, bin index to count.
    pub latency: BTreeMap<u8, u64>,
}

impl Bucket {
    fn add(&mut self, status: u16, duration_ns: u64) {
        self.requests += 1;
        if (100..600).contains(&status) {
            self.status[(status / 100 - 1) as usize] += 1;
        }
        *self.latency.entry(latency_bin(duration_ns)).or_default() += 1;
    }

    fn merge(&mut self, other: &Bucket) {
        self.requests += other.requests;
        for (a, b) in self.status.iter_mut().zip(other.status) {
            *a += b;
        }
        for (bin, n) in &other.latency {
            *self.latency.entry(*bin).or_default() += n;
        }
    }

    /// Latency at quantile `q` (0..1) in ms, from the histogram.
    pub fn quantile_ms(&self, q: f64) -> Option<f64> {
        let total: u64 = self.latency.values().sum();
        if total == 0 {
            return None;
        }
        let rank = ((q * total as f64).ceil() as u64).max(1);
        let mut seen = 0;
        for (bin, n) in &self.latency {
            seen += n;
            if seen >= rank {
                return Some(bin_ms(*bin));
            }
        }
        self.latency.keys().last().map(|b| bin_ms(*b))
    }

    fn to_json(&self) -> Value {
        let round = |v: Option<f64>| v.map(|x| (x * 100.0).round() / 100.0);
        json!({
            "requests": self.requests,
            "status": { "1xx": self.status[0], "2xx": self.status[1], "3xx": self.status[2], "4xx": self.status[3], "5xx": self.status[4] },
            "p50_ms": round(self.quantile_ms(0.5)),
            "p95_ms": round(self.quantile_ms(0.95)),
        })
    }
}

/// Container samples of one hour (or, merged, of a longer period).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub samples: u32,
    /// Samples with at least one replica running.
    pub up: u32,
    /// Samples with every wanted replica running.
    pub full: u32,
    pub mem_sum: u64,
    pub mem_max: u64,
    pub cpu_sum: f64,
}

impl Usage {
    fn add(&mut self, running: u32, wanted: u32, mem: u64, cpu: f64) {
        self.samples += 1;
        self.up += u32::from(running > 0);
        self.full += u32::from(running >= wanted.max(1));
        self.mem_sum += mem;
        self.mem_max = self.mem_max.max(mem);
        self.cpu_sum += cpu;
    }

    fn merge(&mut self, o: &Usage) {
        self.samples += o.samples;
        self.up += o.up;
        self.full += o.full;
        self.mem_sum += o.mem_sum;
        self.mem_max = self.mem_max.max(o.mem_max);
        self.cpu_sum += o.cpu_sum;
    }

    /// Fractions and averages; `null` where nothing was sampled.
    fn to_json(&self) -> Value {
        if self.samples == 0 {
            return json!({ "uptime": null, "full_uptime": null, "mem_avg": null, "mem_max": null, "cpu_avg": null });
        }
        let n = self.samples as f64;
        let round = |x: f64, d: f64| (x * d).round() / d;
        json!({
            "uptime": round(self.up as f64 / n, 10_000.0),
            "full_uptime": round(self.full as f64 / n, 10_000.0),
            "mem_avg": self.mem_sum / self.samples as u64,
            "mem_max": self.mem_max,
            "cpu_avg": round(self.cpu_sum / n, 100.0),
        })
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ServiceStats {
    /// Hour index (unix time / 3600) to bucket.
    hours: BTreeMap<u64, Bucket>,
    /// Hour index to container samples.
    #[serde(default)]
    usage: BTreeMap<u64, Usage>,
    /// Requests per minute for the last hour, as (minute index, count) slots.
    /// Not persisted: it only feeds the live request rate.
    #[serde(skip)]
    minutes: Vec<(u64, u64)>,
}

impl ServiceStats {
    fn record_minute(&mut self, minute: u64) {
        if self.minutes.len() < 60 {
            self.minutes.resize(60, (u64::MAX, 0));
        }
        let slot = &mut self.minutes[(minute % 60) as usize];
        if slot.0 != minute {
            *slot = (minute, 0);
        }
        slot.1 += 1;
    }

    fn last_hour(&self, now: u64) -> u64 {
        let now_min = now / 60;
        self.minutes.iter().filter(|(m, _)| *m <= now_min && now_min - *m < 60).map(|(_, n)| n).sum()
    }
}

/// One line of Traefik's JSON access log, only the fields we keep.
#[derive(Deserialize)]
struct LogLine {
    #[serde(rename = "ServiceName")]
    service: Option<String>,
    #[serde(rename = "DownstreamStatus")]
    status: Option<u16>,
    #[serde(rename = "Duration")]
    duration_ns: Option<u64>,
    #[serde(rename = "StartUTC")]
    start: Option<String>,
}

/// `shop-service@docker` to `shop`; the daemon's own routes are not a service.
fn service_of(traefik_service: &str) -> Option<&str> {
    let name = traefik_service.strip_suffix("@docker")?.strip_suffix("-service")?;
    is_valid_name(name).then_some(name)
}

pub struct StatsStore {
    dir: PathBuf,
    retention_hours: u64,
    services: HashMap<String, ServiceStats>,
    dirty: HashSet<String>,
}

impl StatsStore {
    pub fn load(paths: &Paths, retention_days: u64) -> StatsStore {
        let dir = stats_dir(paths);
        let mut services = HashMap::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let Some(svc) = name.strip_suffix(".json").filter(|s| is_valid_name(s)) else { continue };
                match std::fs::read(e.path()).map_err(anyhow::Error::from).and_then(|b| Ok(serde_json::from_slice(&b)?)) {
                    Ok(s) => {
                        services.insert(svc.to_string(), s);
                    }
                    Err(err) => tracing::warn!("ignoring unreadable stats file {name}: {err:#}"),
                }
            }
        }
        StatsStore { dir, retention_hours: retention_days * 24, services, dirty: HashSet::new() }
    }

    /// Counts one log line. Returns false for lines that are not a service request.
    fn ingest_line(&mut self, line: &[u8], now: u64) -> bool {
        let Ok(l) = serde_json::from_slice::<LogLine>(line) else { return false };
        let (Some(svc), Some(status)) = (l.service.as_deref().and_then(service_of), l.status) else { return false };
        let start = l.start.as_deref().and_then(parse_rfc3339_utc).unwrap_or(now);
        self.record(svc, start, status, l.duration_ns.unwrap_or(0), now);
        true
    }

    pub fn record(&mut self, service: &str, start: u64, status: u16, duration_ns: u64, now: u64) {
        let hour = start / 3600;
        if hour + self.retention_hours < now / 3600 {
            return;
        }
        let s = self.services.entry(service.to_string()).or_default();
        s.hours.entry(hour).or_default().add(status, duration_ns);
        if now.saturating_sub(start) < 3600 {
            s.record_minute(start / 60);
        }
        self.dirty.insert(service.to_string());
    }

    /// One container sample of a deployed service.
    pub fn record_usage(&mut self, service: &str, t: u64, running: u32, wanted: u32, mem: u64, cpu: f64) {
        let s = self.services.entry(service.to_string()).or_default();
        s.usage.entry(t / 3600).or_default().add(running, wanted, mem, cpu);
        self.dirty.insert(service.to_string());
    }

    /// Drops buckets past the retention and writes changed services to disk.
    pub fn save(&mut self, now: u64) -> Result<()> {
        let oldest = (now / 3600).saturating_sub(self.retention_hours);
        for (name, s) in &mut self.services {
            let before = s.hours.len() + s.usage.len();
            s.hours.retain(|h, _| *h >= oldest);
            s.usage.retain(|h, _| *h >= oldest);
            if s.hours.len() + s.usage.len() != before {
                self.dirty.insert(name.clone());
            }
        }
        if self.dirty.is_empty() {
            return Ok(());
        }
        std::fs::create_dir_all(&self.dir)?;
        for name in std::mem::take(&mut self.dirty) {
            if let Some(s) = self.services.get(&name) {
                write_atomic(&self.dir.join(format!("{name}.json")), &serde_json::to_vec(s)?, 0o600)?;
            }
        }
        Ok(())
    }

    pub fn remove(&mut self, service: &str) {
        self.services.remove(service);
        self.dirty.remove(service);
        let _ = std::fs::remove_file(self.dir.join(format!("{service}.json")));
    }

    pub fn requests_last_hour(&self, service: &str, now: u64) -> u64 {
        self.services.get(service).map(|s| s.last_hour(now)).unwrap_or(0)
    }

    /// Buckets of `step` seconds (an hour or a day) covering the last `since` seconds,
    /// including empty ones so a sparkline has no gaps.
    pub fn query(&self, service: &str, since: u64, by_day: bool, now: u64) -> Value {
        let step = if by_day { 86_400 } else { 3600 };
        let first = (now.saturating_sub(since) / step) * step;
        let mut buckets: BTreeMap<u64, (Bucket, Usage)> = (0..)
            .map(|i| first + i * step)
            .take_while(|t| *t <= now)
            .map(|t| (t, Default::default()))
            .collect();
        let mut total = (Bucket::default(), Usage::default());
        if let Some(s) = self.services.get(service) {
            for (hour, b) in s.hours.range(first / 3600..) {
                if let Some(target) = buckets.get_mut(&((hour * 3600 / step) * step)) {
                    target.0.merge(b);
                    total.0.merge(b);
                }
            }
            for (hour, u) in s.usage.range(first / 3600..) {
                if let Some(target) = buckets.get_mut(&((hour * 3600 / step) * step)) {
                    target.1.merge(u);
                    total.1.merge(u);
                }
            }
        }
        let both = |b: &Bucket, u: &Usage| {
            let mut v = b.to_json();
            if let (Some(o), Value::Object(extra)) = (v.as_object_mut(), u.to_json()) {
                o.extend(extra);
            }
            v
        };
        let list: Vec<Value> = buckets
            .iter()
            .map(|(t, (b, u))| {
                let mut v = both(b, u);
                v["t"] = json!(t);
                v
            })
            .collect();
        json!({
            "service": service,
            "by": if by_day { "day" } else { "hour" },
            "since": since,
            "total": both(&total.0, &total.1),
            "buckets": list,
        })
    }
}

/// Where reading the access log continues after a restart.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Cursor {
    inode: u64,
    offset: u64,
}

fn cursor_path(paths: &Paths) -> PathBuf {
    stats_dir(paths).join("cursor.json")
}

/// Reads complete lines from `offset` to the end; a partial last line is left for next time.
fn read_lines(file: &mut File, offset: &mut u64, store: &Mutex<StatsStore>) -> Result<u64> {
    file.seek(SeekFrom::Start(*offset))?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut counted = 0;
    let now = unix_now();
    loop {
        line.clear();
        let n = reader.by_ref().take(MAX_LINE as u64 + 1).read_until(b'\n', &mut line)?;
        if n == 0 || line.last() != Some(&b'\n') {
            if n > MAX_LINE {
                // An absurdly long line: skip to its end.
                let mut rest = Vec::new();
                let extra = reader.read_until(b'\n', &mut rest)?;
                *offset += (n + extra) as u64;
                continue;
            }
            break;
        }
        *offset += n as u64;
        if store.lock().unwrap_or_else(|e| e.into_inner()).ingest_line(&line, now) {
            counted += 1;
        }
    }
    Ok(counted)
}

/// One pass: read new lines, rotate the log when it is large, persist.
fn tick(paths: &Paths, store: &Mutex<StatsStore>, cursor: &mut Cursor) -> Result<()> {
    tick_with(paths, store, cursor, ROTATE_AT, &signal_router)
}

/// Tells Traefik to reopen its access log, then gives it time to finish
/// writing the lines it had buffered for the old file.
fn signal_router(_old: &Path) -> bool {
    let ok = std::process::Command::new("docker")
        .args(["kill", "--signal", "USR1", ROUTER_CONTAINER])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if ok {
        std::thread::sleep(Duration::from_secs(3));
    }
    ok
}

fn tick_with(
    paths: &Paths,
    store: &Mutex<StatsStore>,
    cursor: &mut Cursor,
    rotate_at: u64,
    reopen: &dyn Fn(&Path) -> bool,
) -> Result<()> {
    let path = log_dir(paths).join(ACCESS_LOG);
    let mut file = match File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("cannot open {}", path.display())),
    };
    let meta = file.metadata()?;
    if meta.ino() != cursor.inode || meta.len() < cursor.offset {
        // A new or truncated file: start from the beginning.
        *cursor = Cursor { inode: meta.ino(), offset: 0 };
    }
    read_lines(&mut file, &mut cursor.offset, store)?;
    // Counts and cursor are saved together before rotating, so a failed
    // rotation or a restart never counts a line twice.
    persist(paths, store, cursor)?;
    if cursor.offset >= rotate_at {
        let rotated = rotate(&path, &mut file, cursor, store, reopen);
        persist(paths, store, cursor)?;
        rotated?;
    }
    Ok(())
}

fn persist(paths: &Paths, store: &Mutex<StatsStore>, cursor: &Cursor) -> Result<()> {
    store.lock().unwrap_or_else(|e| e.into_inner()).save(unix_now())?;
    std::fs::create_dir_all(stats_dir(paths))?;
    write_atomic(&cursor_path(paths), &serde_json::to_vec(cursor)?, 0o600)
}

/// Renames the log and has Traefik reopen it, then reads what Traefik wrote
/// to the old file before it switched, through the handle that is still open.
fn rotate(
    path: &Path,
    file: &mut File,
    cursor: &mut Cursor,
    store: &Mutex<StatsStore>,
    reopen: &dyn Fn(&Path) -> bool,
) -> Result<()> {
    let old = path.with_extension("log.old");
    std::fs::rename(path, &old)?;
    if !reopen(&old) {
        // Traefik is not running: keep the file so nothing is lost.
        std::fs::rename(&old, path)?;
        bail!("cannot signal {ROUTER_CONTAINER} to reopen its access log");
    }
    read_lines(file, &mut cursor.offset, store)?;
    std::fs::remove_file(&old)?;
    *cursor = Cursor::default();
    Ok(())
}

/// Runs forever in the daemon.
pub async fn run(paths: Paths, store: std::sync::Arc<Mutex<StatsStore>>) {
    let mut cursor: Cursor = std::fs::read(cursor_path(&paths))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    loop {
        let (p, s) = (paths.clone(), store.clone());
        let result = tokio::task::spawn_blocking(move || {
            let r = tick(&p, &s, &mut cursor);
            (r, cursor)
        })
        .await;
        match result {
            Ok((r, c)) => {
                cursor = c;
                if let Err(e) = r {
                    tracing::warn!("traffic stats: {e:#}");
                }
            }
            Err(e) => {
                tracing::error!("traffic stats task failed: {e}");
                cursor = Cursor::default();
            }
        }
        tokio::time::sleep(INGEST_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn line(svc: &str, status: u16, ms: u64, start: &str) -> String {
        format!(
            r#"{{"ServiceName":"{svc}","DownstreamStatus":{status},"Duration":{},"StartUTC":"{start}","level":"info","msg":"","time":"x"}}"#,
            ms * 1_000_000
        )
    }

    fn store() -> (tempfile::TempDir, Paths, StatsStore) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { root: dir.path().to_path_buf() };
        let s = StatsStore::load(&paths, 90);
        (dir, paths, s)
    }

    #[test]
    fn service_names() {
        assert_eq!(service_of("shop-service@docker"), Some("shop"));
        assert_eq!(service_of("my-shop-service@docker"), Some("my-shop"));
        assert_eq!(service_of("mypaas-daemon@file"), None);
        assert_eq!(service_of("noise@internal"), None);
        assert_eq!(service_of("Bad-service@docker"), None);
    }

    #[test]
    fn histogram_quantiles_are_close() {
        let mut b = Bucket::default();
        // 90 fast requests at 10ms, 10 slow ones at 500ms.
        for _ in 0..90 {
            b.add(200, 10_000_000);
        }
        for _ in 0..10 {
            b.add(500, 500_000_000);
        }
        let p50 = b.quantile_ms(0.5).unwrap();
        let p95 = b.quantile_ms(0.95).unwrap();
        assert!((p50 - 10.0).abs() / 10.0 < 0.1, "p50 {p50}");
        assert!((p95 - 500.0).abs() / 500.0 < 0.1, "p95 {p95}");
        assert_eq!(b.status, [0, 90, 0, 0, 10]);
        assert_eq!(Bucket::default().quantile_ms(0.5), None);
    }

    #[test]
    fn lines_become_hourly_buckets() {
        let (_d, _p, mut s) = store();
        let now = parse_rfc3339_utc("2026-11-07T12:30:00Z").unwrap();
        assert!(s.ingest_line(line("shop-service@docker", 200, 5, "2026-11-07T11:10:00.5Z").as_bytes(), now));
        assert!(s.ingest_line(line("shop-service@docker", 404, 5, "2026-11-07T11:59:59Z").as_bytes(), now));
        assert!(s.ingest_line(line("shop-service@docker", 200, 5, "2026-11-07T12:00:00Z").as_bytes(), now));
        assert!(!s.ingest_line(line("mypaas-daemon@file", 403, 1, "2026-11-07T12:00:00Z").as_bytes(), now));
        assert!(!s.ingest_line(b"not json", now));
        let q = s.query("shop", 2 * 3600, false, now);
        let b = q["buckets"].as_array().unwrap();
        assert_eq!(b.len(), 3, "10:00, 11:00 and 12:00");
        assert_eq!(b[1]["requests"], 2);
        assert_eq!(b[1]["status"]["4xx"], 1);
        assert_eq!(b[2]["requests"], 1);
        assert_eq!(q["total"]["requests"], 3);
        let day = s.query("shop", 86_400, true, now);
        assert_eq!(day["total"]["requests"], 3);
        assert_eq!(s.query("blog", 3600, false, now)["total"]["requests"], 0);
    }

    #[test]
    fn retention_and_persistence() {
        let (_d, paths, mut s) = store();
        let now = 1_800_000_000;
        s.record("shop", now - 91 * 86_400, 200, 1, now);
        s.record("shop", now - 89 * 86_400, 200, 1, now);
        s.record("shop", now - 10, 200, 1, now);
        s.save(now).unwrap();
        let reloaded = StatsStore::load(&paths, 90);
        assert_eq!(reloaded.query("shop", 100 * 86_400, true, now)["total"]["requests"], 2, "older than 90 days is dropped");
        // Ninety days later the older bucket ages out on save.
        let mut s = reloaded;
        s.save(now + 2 * 86_400).unwrap();
        assert_eq!(StatsStore::load(&paths, 90).query("shop", 100 * 86_400, true, now + 2 * 86_400)["total"]["requests"], 1);
        s.remove("shop");
        assert_eq!(StatsStore::load(&paths, 90).query("shop", 86_400, true, now)["total"]["requests"], 0);
    }

    #[test]
    fn usage_samples_become_uptime_and_memory() {
        let (_d, paths, mut st) = store();
        let now = 1_000 * 3600 + 1800;
        // Hour 1: two replicas wanted, four samples: all up, all up, one up, none.
        let h1 = now - 3600;
        st.record_usage("shop", h1, 2, 2, 100, 10.0);
        st.record_usage("shop", h1 + 60, 2, 2, 300, 30.0);
        st.record_usage("shop", h1 + 120, 1, 2, 200, 20.0);
        st.record_usage("shop", h1 + 180, 0, 2, 0, 0.0);
        st.record_usage("shop", now, 2, 2, 500, 5.0);
        let v = st.query("shop", 2 * 3600, false, now);
        let b = &v["buckets"].as_array().unwrap();
        assert_eq!(b.len(), 3);
        assert_eq!(b[0]["uptime"], Value::Null, "no samples is unknown, not down");
        assert_eq!(b[1]["uptime"], 0.75);
        assert_eq!(b[1]["full_uptime"], 0.5);
        assert_eq!(b[1]["mem_avg"], 150);
        assert_eq!(b[1]["mem_max"], 300);
        assert_eq!(b[1]["cpu_avg"], 15.0);
        assert_eq!(v["total"]["mem_max"], 500);
        assert_eq!(v["total"]["uptime"], 0.8);
        // Persisted with the traffic buckets, and dropped with them.
        st.save(now).unwrap();
        let back = StatsStore::load(&paths, 90);
        assert_eq!(back.query("shop", 2 * 3600, false, now)["total"]["mem_max"], 500);
    }

    #[test]
    fn live_rate_covers_the_last_hour() {
        let (_d, _p, mut s) = store();
        let now = 1_800_000_000;
        s.record("a", now - 30, 200, 1, now);
        s.record("a", now - 59 * 60, 200, 1, now);
        s.record("a", now - 2 * 3600, 200, 1, now);
        assert_eq!(s.requests_last_hour("a", now), 2);
        assert_eq!(s.requests_last_hour("a", now + 3600), 0);
    }

    #[test]
    fn tailing_resumes_and_survives_truncation() {
        let (_d, paths, s) = store();
        let store = Mutex::new(s);
        std::fs::create_dir_all(log_dir(&paths)).unwrap();
        let log = log_dir(&paths).join(ACCESS_LOG);
        let now = chrono_now();
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&log).unwrap();
        writeln!(f, "{}", line("a-service@docker", 200, 1, &now)).unwrap();
        // A line Traefik has not finished writing yet.
        write!(f, "{}", &line("a-service@docker", 200, 1, &now)[..20]).unwrap();
        let mut cursor = Cursor::default();
        tick(&paths, &store, &mut cursor).unwrap();
        assert_eq!(store.lock().unwrap().requests_last_hour("a", unix_now()), 1);
        writeln!(f, "{}", &line("a-service@docker", 200, 1, &now)[20..]).unwrap();
        tick(&paths, &store, &mut cursor).unwrap();
        assert_eq!(store.lock().unwrap().requests_last_hour("a", unix_now()), 2, "the partial line is read once complete");
        tick(&paths, &store, &mut cursor).unwrap();
        assert_eq!(store.lock().unwrap().requests_last_hour("a", unix_now()), 2, "nothing is counted twice");
        // The file is truncated or replaced: read from the start again.
        std::fs::write(&log, format!("{}\n", line("a-service@docker", 500, 1, &now))).unwrap();
        tick(&paths, &store, &mut cursor).unwrap();
        assert_eq!(store.lock().unwrap().requests_last_hour("a", unix_now()), 3);
        // The cursor survives a daemon restart.
        let saved: Cursor = serde_json::from_slice(&std::fs::read(cursor_path(&paths)).unwrap()).unwrap();
        assert_eq!(saved.offset, cursor.offset);
    }

    /// Rotation loses nothing: lines Traefik writes to the old file between the
    /// rename and the reopen are still counted, and nothing is counted twice.
    #[test]
    fn rotation_loses_no_lines() {
        let (_d, paths, s) = store();
        let store = Mutex::new(s);
        std::fs::create_dir_all(log_dir(&paths)).unwrap();
        let log = log_dir(&paths).join(ACCESS_LOG);
        let now = chrono_now();
        let entry = line("a-service@docker", 200, 1, &now);
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&log).unwrap();
        for _ in 0..5 {
            writeln!(f, "{entry}").unwrap();
        }
        // Traefik still holds `f`: it writes two more lines to the renamed file,
        // then reopens and starts a fresh log with one line.
        let reopen = |old: &Path| {
            assert!(old.exists() && !log.exists());
            writeln!(&f, "{entry}").unwrap();
            writeln!(&f, "{entry}").unwrap();
            std::fs::write(&log, format!("{entry}\n")).unwrap();
            true
        };
        let mut cursor = Cursor::default();
        tick_with(&paths, &store, &mut cursor, 100, &reopen).unwrap();
        assert_eq!(store.lock().unwrap().requests_last_hour("a", unix_now()), 7);
        assert!(!log.with_extension("log.old").exists(), "the old file is removed");
        tick_with(&paths, &store, &mut cursor, u64::MAX, &|_: &Path| unreachable!()).unwrap();
        assert_eq!(store.lock().unwrap().requests_last_hour("a", unix_now()), 8, "the new file is read from the start");

        // When Traefik cannot be told to reopen, the file stays where it was.
        for _ in 0..5 {
            std::fs::OpenOptions::new().append(true).open(&log).unwrap().write_all(format!("{entry}\n").as_bytes()).unwrap();
        }
        assert!(tick_with(&paths, &store, &mut cursor, 100, &|_: &Path| false).is_err());
        assert!(log.exists());
        assert_eq!(store.lock().unwrap().requests_last_hour("a", unix_now()), 13);
        // ...and what was counted is on disk with a matching cursor, so a
        // restarted daemon neither loses nor recounts those lines.
        let saved: Cursor = serde_json::from_slice(&std::fs::read(cursor_path(&paths)).unwrap()).unwrap();
        assert_eq!(saved.offset, std::fs::metadata(&log).unwrap().len());
        let reloaded = StatsStore::load(&paths, 90);
        assert_eq!(reloaded.query("a", 3600, false, unix_now())["total"]["requests"], 13);
    }

    fn chrono_now() -> String {
        let now = unix_now();
        let (d, t) = (crate::auth::format_date(now), now % 86_400);
        format!("{d}T{:02}:{:02}:{:02}Z", t / 3600, (t % 3600) / 60, t % 60)
    }
}

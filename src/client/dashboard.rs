//! `mypaas dashboard`: uptime, memory and request history as terminal charts.
//!
//! Without a service it shows one row per service with sparklines; with a
//! service it draws full charts. `--watch` redraws full-screen, like `top`.

use std::io::IsTerminal;
use std::time::Duration;

use anyhow::{Result, bail};
use reqwest::Method;
use serde_json::Value;

use super::chart::{self, AXIS, Agg};
use super::ops::{Screen, check_service, format_time, health_color, wait_for_quit};
use super::{Api, BOLD, DIM, Output, RED, human, paint};
use crate::auth;
use crate::spec::parse_duration_ms;

const WATCH_INTERVAL: Duration = Duration::from_secs(10);
const CHART_HEIGHT: usize = 5;
/// The server serves hourly buckets for at most 14 days.
const MAX_HOURLY: u64 = 14 * 86_400;

pub struct DashboardArgs {
    pub service: Option<String>,
    pub since: String,
    pub watch: bool,
}

pub async fn dashboard(remote: Option<&str>, args: DashboardArgs, out: Output) -> Result<()> {
    if let Some(s) = &args.service {
        check_service(s)?;
    }
    let since_ms = parse_duration_ms(&args.since)?;
    if !(3_600_000..=3650 * 86_400_000).contains(&since_ms) {
        bail!("--since must be between 1h and 3650d");
    }
    let by = if since_ms / 1000 <= MAX_HOURLY { "hour" } else { "day" };
    let api = Api::connect(remote)?;
    let q = Query { since: &args.since, by };

    if args.watch {
        if out.json || !std::io::stdout().is_terminal() {
            bail!("--watch needs a terminal; without it the dashboard is printed once");
        }
        let screen = Screen::enter()?;
        loop {
            let mut lines = vec![paint(out, DIM, &format!("{}  refreshes every 10s, q quits", api.base()))];
            match render(&api, args.service.as_deref(), &q, out, width()).await {
                Ok(l) => lines.extend(l),
                Err(e) if e.downcast_ref::<super::Exit>().is_some_and(|x| x.code == super::EXIT_DENIED) => {
                    drop(screen);
                    return Err(e);
                }
                Err(e) => lines.push(paint(out, RED, &format!("{e:#}"))),
            }
            screen.draw(&lines)?;
            if tokio::task::spawn_blocking(|| wait_for_quit(WATCH_INTERVAL)).await?? {
                return Ok(());
            }
        }
    }

    if out.json {
        let services = get(&api, "/v1/services").await?;
        let names: Vec<String> = match &args.service {
            Some(s) => vec![s.clone()],
            None => service_names(&services),
        };
        let stats = fetch_stats(&api, &names, &q).await?;
        println!("{}", serde_json::json!({ "services": services["services"], "stats": stats }));
        return Ok(());
    }
    for l in render(&api, args.service.as_deref(), &q, out, width()).await? {
        println!("{l}");
    }
    Ok(())
}

struct Query<'a> {
    since: &'a str,
    by: &'a str,
}

fn width() -> usize {
    match crossterm::terminal::size() {
        Ok((c, _)) if c >= 40 && std::io::stdout().is_terminal() => c as usize,
        _ => 100,
    }
}

async fn get(api: &Api, path: &str) -> Result<Value> {
    Ok(api.signed(Method::GET, path, Vec::new()).await?.json().await?)
}

fn service_names(services: &Value) -> Vec<String> {
    services["services"].as_array().into_iter().flatten().filter_map(|s| s["service"].as_str().map(str::to_string)).collect()
}

async fn fetch_stats(api: &Api, names: &[String], q: &Query<'_>) -> Result<Vec<Value>> {
    let calls = names.iter().map(|n| {
        let path = format!("/v1/services/{n}/stats?since={}&by={}", q.since, q.by);
        async move { get(api, &path).await }
    });
    futures_util::future::join_all(calls).await.into_iter().collect()
}

async fn render(api: &Api, service: Option<&str>, q: &Query<'_>, out: Output, width: usize) -> Result<Vec<String>> {
    let services = get(api, "/v1/services").await?;
    match service {
        None => {
            let names = service_names(&services);
            let stats = fetch_stats(api, &names, q).await?;
            Ok(overview(&services, &stats, q, out, width))
        }
        Some(name) => {
            let row = services["services"].as_array().into_iter().flatten().find(|s| s["service"] == name);
            let Some(row) = row else { bail!("no service named {name}") };
            let stats = get(api, &format!("/v1/services/{name}/stats?since={}&by={}", q.since, q.by)).await?;
            Ok(detail(row, &stats, q, out, width))
        }
    }
}

/// The per-bucket series of one stats document.
struct Series {
    times: Vec<u64>,
    requests: Vec<Option<f64>>,
    mem: Vec<Option<f64>>,
    up: Vec<Option<f64>>,
    full: Vec<Option<f64>>,
}

impl Series {
    fn from(stats: &Value) -> Series {
        let buckets: Vec<&Value> = stats["buckets"].as_array().map(|a| a.iter().collect()).unwrap_or_default();
        let col = |key: &str| buckets.iter().map(|b| b[key].as_f64()).collect::<Vec<_>>();
        Series {
            times: buckets.iter().map(|b| b["t"].as_u64().unwrap_or(0)).collect(),
            requests: col("requests"),
            mem: col("mem_max"),
            up: col("uptime"),
            full: col("full_uptime"),
        }
    }

    /// Fits the series into `cols` columns; returns the series and how many
    /// buckets each column holds.
    fn fit(self, cols: usize) -> (Series, usize) {
        let n = self.times.len();
        if n <= cols || cols == 0 {
            return (self, 1);
        }
        let group = n.div_ceil(cols);
        let times = self.times.chunks(group).map(|c| c[0]).collect();
        let s = Series {
            times,
            requests: chart::resample(&self.requests, cols, Agg::Mean),
            mem: chart::resample(&self.mem, cols, Agg::Max),
            // The worst part of a column is what counts for uptime.
            up: chart::resample(&self.up, cols, Agg::Min),
            full: chart::resample(&self.full, cols, Agg::Min),
        };
        (s, group)
    }
}

/// Two header lines: the service and window, then the totals.
fn summary(stats: &Value, row: &Value, q: &Query<'_>, out: Output) -> [String; 2] {
    let t = &stats["total"];
    let n = t["requests"].as_u64().unwrap_or(0);
    let five = t["status"]["5xx"].as_u64().unwrap_or(0);
    let health = row["health"].as_str().unwrap_or("?");
    let err = if n == 0 { "-".to_string() } else { format!("{:.2}%", five as f64 * 100.0 / n as f64) };
    [
        format!(
            "{}  {} {}/{}  {}",
            paint(out, BOLD, row["service"].as_str().unwrap_or("?")),
            paint(out, health_color(health), health),
            row["running"],
            row["wanted"],
            paint(out, DIM, &format!("last {} by {}", q.since, q.by)),
        ),
        format!(
        "uptime {}  requests {}  5xx {}  p95 {}  mem peak {}",
        chart::percent(t["uptime"].as_f64()),
        n,
        if five > 0 { paint(out, RED, &err) } else { err },
        t["p95_ms"].as_f64().map(|v| format!("{v:.0}ms")).unwrap_or_else(|| "-".into()),
        t["mem_max"].as_u64().map(human).unwrap_or_else(|| "-".into()),
        ),
    ]
}

/// `512K`, `24M`, `1.5G`: short enough for the chart's axis.
fn short_bytes(v: f64) -> String {
    let units = ["B", "K", "M", "G", "T"];
    let mut v = v;
    let mut u = 0;
    while v >= 1024.0 && u < units.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if v < 10.0 && u > 0 && v.fract() >= 0.05 { format!("{v:.1}{}", units[u]) } else { format!("{v:.0}{}", units[u]) }
}

fn time_label(by: &str) -> impl Fn(u64) -> String {
    let daily = by == "day";
    move |t| {
        // Days, and midnight on an hourly axis, are shown as the date.
        if daily || t % 86_400 == 0 {
            auth::format_date(t)[5..].to_string()
        } else {
            format!("{:02}:00", (t % 86_400) / 3600)
        }
    }
}

/// Full charts for one service.
fn detail(row: &Value, stats: &Value, q: &Query<'_>, out: Output, width: usize) -> Vec<String> {
    let cols = width.saturating_sub(AXIS + 1).max(10);
    let (s, group) = Series::from(stats).fit(cols);
    let col_width = (cols / s.times.len().max(1)).clamp(1, 3);
    let per = match (q.by, group) {
        ("hour", 1) => "per hour".to_string(),
        ("hour", g) => format!("per hour, {g}h per column"),
        (_, 1) => "per day".to_string(),
        (_, g) => format!("per day, {g}d per column"),
    };
    let mut lines: Vec<String> = summary(stats, row, q, out).into();
    lines.push(String::new());

    lines.push(format!("{}  {}", paint(out, BOLD, "requests"), paint(out, DIM, &per)));
    lines.extend(chart::bars(&s.requests, CHART_HEIGHT, col_width, chart::compact).into_iter().map(|l| color_body(out, &l, "36")));
    lines.push(chart::time_axis(&s.times, col_width, time_label(q.by)));
    if s.requests.iter().flatten().all(|&r| r == 0.0) {
        lines.push(paint(out, DIM, "no requests in this period (counted from Traefik's access log)"));
    }
    lines.push(String::new());

    lines.push(format!("{}  {}", paint(out, BOLD, "memory"), paint(out, DIM, "peak of all replicas")));
    lines.extend(
        chart::bars(&s.mem, CHART_HEIGHT, col_width, short_bytes).into_iter().map(|l| color_body(out, &l, "35")),
    );
    lines.push(chart::time_axis(&s.times, col_width, time_label(q.by)));
    lines.push(String::new());

    let strip: String = s.up.iter().zip(&s.full).map(|(u, f)| chart::uptime_cell(out, *u, *f, col_width)).collect();
    lines.push(format!("{}  {}", paint(out, BOLD, "uptime"), paint(out, DIM, "worst part of each column")));
    lines.push(format!("{:>w$} │{strip}", chart::percent(stats["total"]["uptime"].as_f64()), w = AXIS - 2));
    lines.push(chart::time_axis(&s.times, col_width, time_label(q.by)));
    lines.push(chart::uptime_legend(out));
    if s.up.iter().all(Option::is_none) {
        lines.push(paint(out, DIM, "no samples yet: the daemon records uptime and memory once a minute"));
    }
    lines
}

/// Colours a chart row after its axis labels.
fn color_body(out: Output, line: &str, code: &str) -> String {
    match line.char_indices().nth(AXIS) {
        Some((i, _)) => format!("{}{}", &line[..i], paint(out, code, &line[i..])),
        None => line.to_string(),
    }
}

/// One row per service with sparklines.
fn overview(services: &Value, stats: &[Value], q: &Query<'_>, out: Output, width: usize) -> Vec<String> {
    let rows: Vec<&Value> = services["services"].as_array().map(|a| a.iter().collect()).unwrap_or_default();
    // Name, health, replicas, uptime, requests and memory take about 70 columns.
    let spark = width.saturating_sub(72).clamp(8, 48) / 2;
    let mut lines = vec![
        paint(out, DIM, &format!("last {} by {}, {}", q.since, q.by, format_time(auth::unix_now()))),
        paint(
            out,
            BOLD,
            &format!(
                "{:<22} {:<10} {:<8} {:>8} {:>9} {:>10}  {:<spark$} {:<spark$}",
                "SERVICE", "HEALTH", "REPLICAS", "UPTIME", "REQUESTS", "MEM PEAK", "REQUESTS", "UPTIME"
            ),
        ),
    ];
    for (row, st) in rows.iter().zip(stats) {
        let s = Series::from(st).fit(spark).0;
        let health = row["health"].as_str().unwrap_or("?");
        let t = &st["total"];
        let strip: String = s.up.iter().zip(&s.full).map(|(u, f)| chart::uptime_cell(out, *u, *f, 1)).collect();
        lines.push(format!(
            "{:<22} {} {:<8} {:>8} {:>9} {:>10}  {} {}",
            row["service"].as_str().unwrap_or("?"),
            paint(out, health_color(health), &format!("{health:<10}")),
            format!("{}/{}", row["running"], row["wanted"]),
            chart::percent(t["uptime"].as_f64()),
            t["requests"].as_u64().unwrap_or(0),
            t["mem_max"].as_u64().map(human).unwrap_or_else(|| "-".into()),
            paint(out, "36", &format!("{:<spark$}", chart::sparkline(&s.requests))),
            strip,
        ));
    }
    if rows.is_empty() {
        lines.push(paint(out, DIM, "no services deployed yet"));
    } else {
        lines.push(String::new());
        lines.push(chart::uptime_legend(out));
        lines.push(paint(out, DIM, "details: mypaas dashboard <service>"));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats() -> Value {
        let buckets: Vec<Value> = (0..25)
            .map(|i| {
                serde_json::json!({
                    "t": 1_000 * 3600 + i * 3600, "requests": i * 10, "status": { "5xx": 0 },
                    "mem_max": if i < 3 { Value::Null } else { (i * 1_048_576).into() },
                    "uptime": if i < 3 { Value::Null } else if i == 10 { 0.5.into() } else { 1.0.into() },
                    "full_uptime": if i < 3 { Value::Null } else { 1.0.into() },
                })
            })
            .collect();
        serde_json::json!({ "buckets": buckets, "total": { "requests": 3000, "uptime": 0.98, "mem_max": 25165824, "p95_ms": 12.0, "status": { "5xx": 3 } } })
    }

    fn row() -> Value {
        serde_json::json!({ "service": "shop", "health": "healthy", "running": 2, "wanted": 2 })
    }

    #[test]
    fn detail_draws_three_charts() {
        let q = Query { since: "24h", by: "hour" };
        let lines = detail(&row(), &stats(), &q, Output::default(), 100);
        let text = lines.join("\n");
        assert!(lines[0].starts_with("shop  healthy 2/2"), "{}", lines[0]);
        assert!(lines[1].contains("uptime 98.00%") && lines[1].contains("requests 3000") && lines[1].contains("mem peak 24.0 MiB"), "{}", lines[1]);
        assert!(text.contains("\n   24M ┤"), "memory axis labels stay aligned:\n{text}");
        assert!(text.contains("requests  per hour") && text.contains("memory") && text.contains("uptime"));
        assert!(text.contains('█') && text.contains('▃'), "the half-down hour shows");
        assert!(text.contains("16:00"), "hour labels on the axis:\n{text}");
        assert!(!text.contains('\x1b'), "no colour without a terminal");
        assert!(lines.iter().all(|l| l.chars().count() <= 100), "fits the width");
    }

    #[test]
    fn narrow_terminals_get_grouped_columns() {
        let q = Query { since: "24h", by: "hour" };
        let text = detail(&row(), &stats(), &q, Output::default(), 20).join("\n");
        assert!(text.contains("3h per column"), "{text}");
    }

    #[test]
    fn short_byte_labels() {
        assert_eq!(short_bytes(0.0), "0B");
        assert_eq!(short_bytes(512.0 * 1024.0), "512K");
        assert_eq!(short_bytes(24.0 * 1048576.0), "24M");
        assert_eq!(short_bytes(1.5 * 1073741824.0), "1.5G");
    }

    #[test]
    fn overview_has_a_row_per_service() {
        let q = Query { since: "24h", by: "hour" };
        let services = serde_json::json!({ "services": [row()] });
        let lines = overview(&services, &[stats()], &q, Output::default(), 120);
        assert!(lines[1].starts_with("SERVICE"));
        assert!(lines[2].starts_with("shop") && lines[2].contains("98.00%") && lines[2].contains("2/2"), "{}", lines[2]);
        let empty = overview(&serde_json::json!({ "services": [] }), &[], &q, Output::default(), 120);
        assert!(empty.iter().any(|l| l.contains("no services")));
    }
}

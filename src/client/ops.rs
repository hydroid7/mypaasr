//! Day-to-day commands that replace the dashboard: ls, status, logs, top, secrets, doctor.

use std::io::{IsTerminal, Read, Write};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use reqwest::Method;
use serde_json::Value;

use super::{Api, BOLD, DIM, EXIT_DENIED, EXIT_ERROR, GREEN, Output, RED, YELLOW, exit, human, paint};
use crate::auth;
use crate::spec::{is_valid_env_name, is_valid_name, parse_duration_ms};

const TOP_INTERVAL: Duration = Duration::from_secs(2);
const MAX_SECRET_VALUE: usize = 16 * 1024;

fn check_service(name: &str) -> Result<()> {
    if !is_valid_name(name) {
        bail!("invalid service name {name:?}");
    }
    Ok(())
}

/// `45s`, `12m`, `5h`, `3d`.
pub fn age(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86_400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

fn health_color(h: &str) -> &'static str {
    match h {
        "healthy" | "running" => GREEN,
        "down" => RED,
        _ => YELLOW,
    }
}

/// Pads before colouring so ANSI codes do not break the column widths.
fn cell(out: Output, code: Option<&str>, text: &str, width: usize) -> String {
    let padded = format!("{text:<width$}");
    match code {
        Some(c) => paint(out, c, &padded),
        None => padded,
    }
}

fn service_table(v: &Value, out: Output) -> Vec<String> {
    let mut lines = vec![paint(
        out,
        BOLD,
        &format!("{:<24} {:<8} {:<10} {:<5} {:>8} {:>7} {:>10}  URLS", "SERVICE", "REPLICAS", "HEALTH", "AGE", "REQ/1H", "CPU", "MEM"),
    )];
    let now = auth::unix_now();
    for s in v["services"].as_array().into_iter().flatten() {
        let health = s["health"].as_str().unwrap_or("?");
        let urls: Vec<&str> = s["urls"].as_array().into_iter().flatten().filter_map(|u| u.as_str()).collect();
        lines.push(format!(
            "{} {} {} {:<5} {:>8} {:>7} {:>10}  {}",
            cell(out, None, s["service"].as_str().unwrap_or("?"), 24),
            cell(out, None, &format!("{}/{}", s["running"], s["wanted"]), 8),
            cell(out, Some(health_color(health)), health, 10),
            age(now.saturating_sub(s["deployed_at"].as_u64().unwrap_or(now))),
            s["requests_1h"].as_u64().unwrap_or(0),
            format!("{:.1}%", s["cpu_percent"].as_f64().unwrap_or(0.0)),
            human(s["mem_bytes"].as_u64().unwrap_or(0)),
            urls.join(" ")
        ));
    }
    if lines.len() == 1 {
        lines.push(paint(out, DIM, "no services deployed yet"));
    }
    lines
}

pub async fn ls(remote: Option<&str>, out: Output) -> Result<()> {
    let api = Api::connect(remote)?;
    let v: Value = api.signed(Method::GET, "/v1/services", Vec::new()).await?.json().await?;
    if out.json {
        println!("{v}");
    } else {
        for l in service_table(&v, out) {
            println!("{l}");
        }
    }
    Ok(())
}

fn format_time(unix: u64) -> String {
    format!("{} {:02}:{:02} UTC", auth::format_date(unix), (unix % 86_400) / 3600, (unix % 3600) / 60)
}

pub async fn status(remote: Option<&str>, service: &str, out: Output) -> Result<()> {
    check_service(service)?;
    let api = Api::connect(remote)?;
    let v: Value = api.signed(Method::GET, &format!("/v1/services/{service}"), Vec::new()).await?.json().await?;
    if out.json {
        println!("{v}");
        return Ok(());
    }
    let strs = |key: &str| -> String {
        let items: Vec<&str> = v[key].as_array().into_iter().flatten().filter_map(|x| x.as_str()).collect();
        if items.is_empty() { "-".into() } else { items.join(", ") }
    };
    let row = |k: &str, val: String| println!("{} {val}", paint(out, BOLD, &format!("{k:<11}")));
    let health = v["health"].as_str().unwrap_or("?");
    row("service", v["service"].as_str().unwrap_or(service).to_string());
    row("urls", strs("urls"));
    let mode = if v["scale"].as_u64() == Some(0) { "single".to_string() } else { v["strategy"].as_str().unwrap_or("?").to_string() };
    row("replicas", format!("{}/{} ({mode}), {}", v["running"], v["wanted"], paint(out, health_color(health), health)));
    row("port", v["port"].to_string());
    let last = &v["last_deploy"];
    row(
        "deployed",
        format!(
            "{} by {} ({})",
            format_time(last["time"].as_u64().unwrap_or(0)),
            last["key_name"].as_str().unwrap_or("?"),
            last["deploy_id"].as_str().unwrap_or("?")
        ),
    );
    row(
        "previous",
        match v["previous"].as_object() {
            Some(p) => format!("{} (mypaas rollback {service})", p["deploy_id"].as_str().unwrap_or("?")),
            None => "none".into(),
        },
    );
    row("networks", strs("networks"));
    row("env", strs("env"));
    let volumes: Vec<String> = v["volumes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|vol| {
            format!(
                "{} -> {}{} ({})",
                vol["source"].as_str().unwrap_or("?"),
                vol["target"].as_str().unwrap_or("?"),
                if vol["readonly"].as_bool() == Some(true) { " ro" } else { "" },
                vol["host"].as_str().unwrap_or("?")
            )
        })
        .collect();
    row("volumes", if volumes.is_empty() { "-".into() } else { volumes.join("\n            ") });
    println!();
    println!("{}", paint(out, BOLD, &format!("{:<28} {:<10} {:<24} {:>7} {:>10}", "CONTAINER", "STATE", "STATUS", "CPU", "MEM")));
    for c in v["containers"].as_array().into_iter().flatten() {
        let state = c["state"].as_str().unwrap_or("?");
        println!(
            "{:<28} {} {:<24} {:>7} {:>10}",
            c["name"].as_str().unwrap_or("?"),
            cell(out, Some(if state == "running" { GREEN } else { RED }), state, 10),
            c["status"].as_str().unwrap_or(""),
            format!("{:.1}%", c["cpu_percent"].as_f64().unwrap_or(0.0)),
            human(c["mem_bytes"].as_u64().unwrap_or(0))
        );
    }
    Ok(())
}

pub struct LogArgs {
    pub follow: bool,
    pub tail: Option<u64>,
    pub since: Option<String>,
    pub container: Option<u32>,
}

/// `▁▂▃▅▇` from the values, scaled to the largest.
pub fn sparkline(values: &[u64]) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let max = values.iter().copied().max().unwrap_or(0);
    values
        .iter()
        .map(|&v| if max == 0 || v == 0 { ' ' } else { BARS[((v * 7).div_ceil(max)) as usize] })
        .collect()
}

fn percent(part: u64, total: u64) -> String {
    if total == 0 { "-".into() } else { format!("{:.1}%", part as f64 * 100.0 / total as f64) }
}

fn ms(v: &Value) -> String {
    match v.as_f64() {
        None => "-".into(),
        Some(x) if x < 10.0 => format!("{x:.1}ms"),
        Some(x) if x < 1000.0 => format!("{x:.0}ms"),
        Some(x) => format!("{:.1}s", x / 1000.0),
    }
}

pub async fn stats(remote: Option<&str>, service: &str, since: &str, by: Option<&str>, out: Output) -> Result<()> {
    check_service(service)?;
    parse_duration_ms(since)?;
    let mut path = format!("/v1/services/{service}/stats?since={since}");
    if let Some(b) = by {
        if b != "hour" && b != "day" {
            bail!("--by must be hour or day");
        }
        path.push_str(&format!("&by={b}"));
    }
    let api = Api::connect(remote)?;
    let v: Value = api.signed(Method::GET, &path, Vec::new()).await?.json().await?;
    if out.json {
        println!("{v}");
        return Ok(());
    }
    let by_day = v["by"] == "day";
    let total = &v["total"];
    let n = total["requests"].as_u64().unwrap_or(0);
    let class = |b: &Value, c: &str| b["status"][c].as_u64().unwrap_or(0);
    let buckets: Vec<&Value> = v["buckets"].as_array().map(|a| a.iter().collect()).unwrap_or_default();
    println!(
        "{}  last {since} by {}  {} requests  2xx {}  4xx {}  5xx {}  p50 {}  p95 {}",
        paint(out, BOLD, service),
        if by_day { "day" } else { "hour" },
        n,
        percent(class(total, "2xx"), n),
        percent(class(total, "4xx"), n),
        paint(out, if class(total, "5xx") > 0 { RED } else { GREEN }, &percent(class(total, "5xx"), n)),
        ms(&total["p50_ms"]),
        ms(&total["p95_ms"]),
    );
    let counts: Vec<u64> = buckets.iter().map(|b| b["requests"].as_u64().unwrap_or(0)).collect();
    let errors: Vec<u64> = buckets.iter().map(|b| class(b, "5xx")).collect();
    println!("requests {}", sparkline(&counts));
    if errors.iter().any(|&e| e > 0) {
        println!("5xx      {}", paint(out, RED, &sparkline(&errors)));
    }
    println!();
    println!("{}", paint(out, BOLD, &format!("{:<17} {:>8} {:>7} {:>7} {:>7} {:>7} {:>8} {:>8}", "TIME (UTC)", "REQS", "2XX", "3XX", "4XX", "5XX", "P50", "P95")));
    for b in buckets.iter().filter(|b| b["requests"].as_u64() != Some(0)) {
        let t = b["t"].as_u64().unwrap_or(0);
        let when = if by_day { auth::format_date(t) } else { format_time(t).trim_end_matches(" UTC").to_string() };
        println!(
            "{when:<17} {:>8} {:>7} {:>7} {:>7} {:>7} {:>8} {:>8}",
            b["requests"].as_u64().unwrap_or(0),
            class(b, "2xx"),
            class(b, "3xx"),
            class(b, "4xx"),
            class(b, "5xx"),
            ms(&b["p50_ms"]),
            ms(&b["p95_ms"])
        );
    }
    if n == 0 {
        println!("{}", paint(out, DIM, "no requests in this period"));
    }
    Ok(())
}

/// Calls `f` for each complete line of a streamed body.
async fn each_line(resp: reqwest::Response, mut f: impl FnMut(&str)) -> Result<()> {
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        buf.extend_from_slice(&chunk.context("connection lost")?);
        while let Some(i) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=i).collect();
            f(String::from_utf8_lossy(&line[..line.len() - 1]).as_ref());
        }
    }
    if !buf.is_empty() {
        f(String::from_utf8_lossy(&buf).as_ref());
    }
    Ok(())
}

pub async fn logs(remote: Option<&str>, service: &str, args: LogArgs, out: Output) -> Result<()> {
    check_service(service)?;
    let mut query = Vec::new();
    if args.follow {
        query.push("follow=1".to_string());
    }
    if let Some(t) = args.tail {
        query.push(format!("tail={t}"));
    }
    if let Some(s) = &args.since {
        parse_duration_ms(s)?;
        query.push(format!("since={s}"));
    }
    if let Some(c) = args.container {
        query.push(format!("container={c}"));
    }
    let mut path = format!("/v1/services/{service}/logs");
    if !query.is_empty() {
        path = format!("{path}?{}", query.join("&"));
    }
    let api = Api::connect(remote)?;
    let resp = api.signed(Method::GET, &path, Vec::new()).await?;
    let stdout = std::io::stdout();
    each_line(resp, |line| {
        let mut lock = stdout.lock();
        if out.json {
            let _ = writeln!(lock, "{line}");
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else { return };
        let name = v["container"].as_str().unwrap_or("?");
        if let Some(e) = v["error"].as_str() {
            let _ = writeln!(lock, "{} {}", paint(out, DIM, &format!("[{name}]")), paint(out, RED, e));
        } else {
            let _ = writeln!(lock, "{} {}", paint(out, DIM, &format!("[{name}]")), v["line"].as_str().unwrap_or(""));
        }
    })
    .await
}

/// Restores the terminal when `top` ends, also on errors.
struct Screen;

impl Screen {
    fn enter() -> Result<Screen> {
        crossterm::terminal::enable_raw_mode()?;
        crossterm::execute!(std::io::stdout(), crossterm::terminal::EnterAlternateScreen, crossterm::cursor::Hide)?;
        Ok(Screen)
    }

    fn draw(&self, lines: &[String]) -> Result<()> {
        use crossterm::{cursor::MoveTo, queue, terminal};
        let mut stdout = std::io::stdout();
        // Some terminals (and fresh ptys) report 0x0.
        let (cols, rows) = match terminal::size() {
            Ok((c, r)) if c > 0 && r > 0 => (c, r),
            _ => (80, 24),
        };
        queue!(stdout, MoveTo(0, 0), terminal::Clear(terminal::ClearType::All))?;
        for (i, l) in lines.iter().take(rows as usize).enumerate() {
            queue!(stdout, MoveTo(0, i as u16))?;
            write!(stdout, "{}", truncate_visible(l, cols as usize))?;
        }
        stdout.flush()?;
        Ok(())
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = crossterm::execute!(std::io::stdout(), crossterm::cursor::Show, crossterm::terminal::LeaveAlternateScreen);
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// Cuts a line to `width` visible characters, keeping ANSI colour codes intact.
fn truncate_visible(line: &str, width: usize) -> String {
    let mut out = String::new();
    let mut visible = 0;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            out.push(c);
            for e in chars.by_ref() {
                out.push(e);
                if e.is_ascii_alphabetic() {
                    break;
                }
            }
        } else if visible < width {
            out.push(c);
            visible += 1;
        }
    }
    out
}

/// Waits up to `d` for `q`, Esc or Ctrl-C. Returns true to quit.
fn wait_for_quit(d: Duration) -> Result<bool> {
    use crossterm::event::{self, Event, KeyCode, KeyModifiers};
    let deadline = std::time::Instant::now() + d;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() || !event::poll(left)? {
            return Ok(false);
        }
        if let Event::Key(k) = event::read()? {
            let ctrl_c = k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL);
            if matches!(k.code, KeyCode::Char('q') | KeyCode::Esc) || ctrl_c {
                return Ok(true);
            }
        }
    }
}

pub async fn top(remote: Option<&str>, out: Output) -> Result<()> {
    let api = Api::connect(remote)?;
    if out.json || !std::io::stdout().is_terminal() {
        loop {
            let v: Value = api.signed(Method::GET, "/v1/services", Vec::new()).await?.json().await?;
            println!("{v}");
            tokio::time::sleep(TOP_INTERVAL).await;
        }
    }
    let screen = Screen::enter()?;
    loop {
        let mut lines = vec![
            format!("{}  {}  {}", paint(out, BOLD, "mypaas top"), api.base(), format_time(auth::unix_now())),
            paint(out, DIM, "refreshes every 2s, q quits"),
            String::new(),
        ];
        match api.signed(Method::GET, "/v1/services", Vec::new()).await {
            Ok(resp) => match resp.json::<Value>().await {
                Ok(v) => lines.extend(service_table(&v, out)),
                Err(e) => lines.push(paint(out, RED, &format!("invalid answer: {e}"))),
            },
            // A denied key will not start working by retrying.
            Err(e) if e.downcast_ref::<super::Exit>().is_some_and(|x| x.code == EXIT_DENIED) => {
                drop(screen);
                return Err(e);
            }
            Err(e) => lines.push(paint(out, RED, &format!("{e:#}"))),
        }
        screen.draw(&lines)?;
        if tokio::task::spawn_blocking(|| wait_for_quit(TOP_INTERVAL)).await?? {
            return Ok(());
        }
    }
}

/// Reads a secret value from stdin when piped, else from a hidden prompt.
fn read_secret_value(key: &str) -> Result<String> {
    let mut value = if std::io::stdin().is_terminal() {
        rpassword::prompt_password(format!("value for {key}: "))?
    } else {
        let mut s = String::new();
        std::io::stdin().take(MAX_SECRET_VALUE as u64 + 3).read_to_string(&mut s)?;
        s
    };
    if value.ends_with('\n') {
        value.pop();
        if value.ends_with('\r') {
            value.pop();
        }
    }
    if value.len() > MAX_SECRET_VALUE {
        bail!("secret values are limited to {MAX_SECRET_VALUE} bytes");
    }
    if value.contains(['\n', '\r', '\0']) {
        bail!("secret values cannot contain line breaks");
    }
    Ok(value)
}

fn check_key(key: &str) -> Result<()> {
    if !is_valid_env_name(key) {
        bail!("invalid secret name {key:?}: use letters, digits and _, not starting with a digit");
    }
    Ok(())
}

pub async fn secret_set(remote: Option<&str>, service: &str, key: &str, out: Output) -> Result<()> {
    check_service(service)?;
    check_key(key)?;
    let api = Api::connect(remote)?;
    let value = read_secret_value(key)?;
    api.signed(Method::PUT, &format!("/v1/services/{service}/secrets/{key}"), value.into_bytes()).await?;
    if !out.quiet {
        eprintln!("set {key} for {service}; it takes effect at the next deploy or rollback");
    }
    Ok(())
}

pub async fn secret_ls(remote: Option<&str>, service: &str, out: Output) -> Result<()> {
    check_service(service)?;
    let api = Api::connect(remote)?;
    let v: Value = api.signed(Method::GET, &format!("/v1/services/{service}/secrets"), Vec::new()).await?.json().await?;
    if out.json {
        println!("{v}");
    } else {
        for n in v["names"].as_array().into_iter().flatten().filter_map(|n| n.as_str()) {
            println!("{n}");
        }
    }
    Ok(())
}

pub async fn secret_rm(remote: Option<&str>, service: &str, key: &str, out: Output) -> Result<()> {
    check_service(service)?;
    check_key(key)?;
    let api = Api::connect(remote)?;
    api.signed(Method::DELETE, &format!("/v1/services/{service}/secrets/{key}"), Vec::new()).await?;
    if !out.quiet {
        eprintln!("removed {key} from {service}; running containers keep it until the next deploy");
    }
    Ok(())
}

struct Check {
    name: &'static str,
    ok: bool,
    detail: String,
}

/// Checks the key, the connection and TLS, the clock and a signed request.
pub async fn doctor(remote: Option<&str>, out: Output) -> Result<()> {
    let mut checks = Vec::new();
    let mut code = 0u8;
    let result = doctor_checks(remote, &mut checks, &mut code).await;
    if out.json {
        let list: Vec<Value> = checks.iter().map(|c| serde_json::json!({ "check": c.name, "ok": c.ok, "detail": c.detail })).collect();
        println!("{}", Value::Array(list));
    } else {
        for c in &checks {
            let mark = if c.ok { paint(out, GREEN, "ok  ") } else { paint(out, RED, "FAIL") };
            println!("{mark} {:<8} {}", c.name, c.detail);
        }
    }
    result?;
    if code != 0 {
        return Err(exit(code, "some checks failed"));
    }
    Ok(())
}

async fn doctor_checks(remote: Option<&str>, checks: &mut Vec<Check>, code: &mut u8) -> Result<()> {
    let mut fail = |checks: &mut Vec<Check>, name, detail: String, c: u8| {
        checks.push(Check { name, ok: false, detail });
        *code = (*code).max(c);
    };
    let (key, key_id) = match super::load_signing_key() {
        Ok(k) => k,
        Err(e) => {
            fail(checks, "key", format!("{e:#}"), EXIT_ERROR);
            return Ok(());
        }
    };
    checks.push(Check { name: "key", ok: true, detail: key_id.clone() });
    let base = match super::resolve_remote(remote) {
        Ok(b) => b,
        Err(e) => {
            fail(checks, "remote", format!("{e:#}"), EXIT_ERROR);
            return Ok(());
        }
    };
    checks.push(Check { name: "remote", ok: true, detail: base.clone() });
    let api = Api::new(base.clone(), key, key_id.clone())?;
    let resp = match api.send_signed(Method::GET, "/v1/services", Vec::new()).await {
        Ok(r) => r,
        Err(e) => {
            fail(checks, "connect", format!("{:#}", anyhow::Error::from(e)), EXIT_ERROR);
            return Ok(());
        }
    };
    if base.starts_with("https://") {
        checks.push(Check { name: "tls", ok: true, detail: "certificate is valid for the admin domain".into() });
    }
    match resp.headers().get("date").and_then(|d| d.to_str().ok()).and_then(|d| httpdate::parse_http_date(d).ok()) {
        Some(server) => {
            let server = server.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            let skew = auth::unix_now() as i64 - server as i64;
            // The server accepts 30s; fail with a margin for network delay.
            match skew.abs() {
                0..=10 => checks.push(Check { name: "clock", ok: true, detail: format!("{skew:+}s from the server") }),
                11..=25 => checks.push(Check {
                    name: "clock",
                    ok: true,
                    detail: format!("{skew:+}s from the server; close to the 30s limit, check NTP"),
                }),
                _ => fail(checks, "clock", format!("{skew:+}s from the server; requests are accepted within 30s"), EXIT_ERROR),
            }
        }
        None => fail(checks, "clock", "the server sent no Date header".into(), EXIT_ERROR),
    }
    let status = resp.status();
    let retry = resp.headers().get("retry-after").and_then(|v| v.to_str().ok()).unwrap_or("?").to_string();
    match status.as_u16() {
        200 => checks.push(Check { name: "auth", ok: true, detail: "a signed GET /v1/services was accepted".into() }),
        403 => fail(
            checks,
            "auth",
            format!("denied: {key_id} is not in authorized_keys, has expired or lacks the read scope"),
            EXIT_DENIED,
        ),
        429 => fail(checks, "auth", format!("locked out or rate limited; retry after {retry}s"), EXIT_DENIED),
        _ => fail(checks, "auth", format!("unexpected answer {status}"), EXIT_ERROR),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages() {
        assert_eq!(age(5), "5s");
        assert_eq!(age(125), "2m");
        assert_eq!(age(7200), "2h");
        assert_eq!(age(3 * 86_400 + 5), "3d");
    }

    #[test]
    fn table_without_colour() {
        let v = serde_json::json!({ "services": [{
            "service": "shop", "running": 2, "wanted": 2, "health": "healthy",
            "cpu_percent": 1.25, "mem_bytes": 1048576, "urls": ["https://shop.example.com"], "deployed_at": 0, "requests_1h": 1234
        }]});
        let lines = service_table(&v, Output::default());
        assert!(lines[0].starts_with("SERVICE"));
        assert!(lines[1].contains("shop") && lines[1].contains("2/2") && lines[1].contains("1.2%") && lines[1].contains("1.0 MiB"));
        assert!(lines[1].contains("1234"));
        assert!(!lines[1].contains('\x1b'));
    }

    #[test]
    fn sparklines() {
        assert_eq!(sparkline(&[0, 1, 2, 4, 8]), " ▂▃▅█");
        assert_eq!(sparkline(&[0, 0]), "  ");
        assert_eq!(sparkline(&[5, 5]), "██");
        assert_eq!(sparkline(&[]), "");
    }

    #[test]
    fn truncation_keeps_colour_codes() {
        assert_eq!(truncate_visible("abcdef", 3), "abc");
        assert_eq!(truncate_visible("\x1b[32mabcdef\x1b[0m", 2), "\x1b[32mab\x1b[0m");
    }
}

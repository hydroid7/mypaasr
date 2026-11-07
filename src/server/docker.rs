//! A thin wrapper around the `docker` CLI. Arguments are passed as a vector,
//! never through a shell.

use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use super::Progress;

pub async fn docker(args: &[&str]) -> Result<String> {
    let out = Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .context("cannot run docker")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("docker {} failed: {}", args.first().unwrap_or(&""), stderr.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Like [`docker`], but returns stdout as raw bytes.
pub async fn docker_bytes(args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("docker").args(args).stdin(Stdio::null()).output().await.context("cannot run docker")?;
    if !out.status.success() {
        bail!("docker {} failed: {}", args.first().unwrap_or(&""), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

/// Like [`docker`], but a failure is only logged.
pub async fn docker_quiet(args: &[&str]) -> bool {
    match docker(args).await {
        Ok(_) => true,
        Err(e) => {
            tracing::debug!("{e:#}");
            false
        }
    }
}

/// Runs docker and sends every output line to `progress`. Kills it after `timeout`.
pub async fn docker_stream(args: &[String], progress: &Progress, timeout: Duration) -> Result<()> {
    let mut child = Command::new("docker")
        .args(args)
        .env("DOCKER_BUILDKIT", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("cannot run docker")?;
    let stdout = child.stdout.take().context("no stdout")?;
    let stderr = child.stderr.take().context("no stderr")?;
    let p1 = progress.clone();
    let p2 = progress.clone();
    let out_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            let _ = p1.send(l);
        }
    });
    let err_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            let _ = p2.send(l);
        }
    });
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(s) => s?,
        Err(_) => {
            let _ = child.kill().await;
            bail!("docker {} took longer than {}s and was stopped", args[0], timeout.as_secs());
        }
    };
    let _ = out_task.await;
    let _ = err_task.await;
    if !status.success() {
        bail!("docker {} failed with {status}", args[0]);
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct ContainerInfo {
    pub name: String,
    pub service: String,
    pub running: bool,
    pub state: String,
    /// Human-readable, e.g. `Up 2 minutes`.
    pub status: String,
}

/// All containers managed by mypaas, optionally only those of one service.
pub async fn list_containers(service: Option<&str>) -> Result<Vec<ContainerInfo>> {
    let filter = match service {
        Some(s) => format!("label=mypaas.service={s}"),
        None => "label=mypaas.service".to_string(),
    };
    let out = docker(&[
        "ps",
        "-a",
        "--no-trunc",
        "--filter",
        &filter,
        "--format",
        "{{.Names}}\t{{.Label \"mypaas.service\"}}\t{{.State}}\t{{.Status}}",
    ])
    .await?;
    Ok(out
        .lines()
        .filter_map(|l| {
            let mut parts = l.split('\t');
            let name = parts.next()?.to_string();
            let service = parts.next()?.to_string();
            let state = parts.next()?.to_string();
            let status = parts.next().unwrap_or("").to_string();
            Some(ContainerInfo { name, service, running: state == "running", state, status })
        })
        .collect())
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Usage {
    pub cpu_percent: f64,
    pub mem_bytes: u64,
}

/// CPU and memory of the named running containers, from one `docker stats` sample.
pub async fn container_stats(names: &[&str]) -> Result<HashMap<String, Usage>> {
    if names.is_empty() {
        return Ok(HashMap::new());
    }
    let mut args = vec!["stats", "--no-stream", "--format", "{{.Name}}\t{{.CPUPerc}}\t{{.MemUsage}}"];
    args.extend_from_slice(names);
    let out = docker(&args).await?;
    Ok(out.lines().filter_map(parse_stats_line).collect())
}

fn parse_stats_line(line: &str) -> Option<(String, Usage)> {
    let mut parts = line.split('\t');
    let name = parts.next()?.to_string();
    let cpu_percent = parts.next()?.trim().trim_end_matches('%').parse().ok()?;
    let mem = parts.next()?.split('/').next()?.trim();
    Some((name, Usage { cpu_percent, mem_bytes: parse_docker_size(mem)? }))
}

/// Parses sizes as docker prints them: `1.5MiB`, `300kB`, `2GB`, `0B`.
fn parse_docker_size(s: &str) -> Option<u64> {
    let i = s.find(|c: char| c.is_ascii_alphabetic())?;
    let (num, unit) = s.split_at(i);
    let v: f64 = num.trim().parse().ok()?;
    let mult: f64 = match unit {
        "B" => 1.0,
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        "GiB" => 1024.0 * 1024.0 * 1024.0,
        "TiB" => 1024.0f64.powi(4),
        "kB" | "KB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        _ => return None,
    };
    Some((v * mult) as u64)
}

pub async fn image_exists(image: &str) -> bool {
    docker(&["image", "inspect", "--format", "{{.Id}}", image]).await.is_ok()
}

pub async fn image_id(image: &str) -> Result<String> {
    Ok(docker(&["image", "inspect", "--format", "{{.Id}}", image]).await?.trim().to_string())
}

pub async fn network_exists(name: &str) -> bool {
    docker(&["network", "inspect", "--format", "{{.Name}}", name]).await.is_ok()
}

pub async fn ensure_network(name: &str, label: &str) -> Result<()> {
    if !network_exists(name).await {
        docker(&["network", "create", "--driver", "bridge", "--label", label, name]).await?;
    }
    Ok(())
}

pub async fn is_running(container: &str) -> bool {
    matches!(
        docker(&["inspect", "--format", "{{.State.Running}}", container]).await.as_deref().map(str::trim),
        Ok("true")
    )
}

pub async fn container_ip(container: &str, network: &str) -> Result<String> {
    let fmt = format!("{{{{(index .NetworkSettings.Networks \"{network}\").IPAddress}}}}");
    let ip = docker(&["inspect", "--format", &fmt, container]).await?.trim().to_string();
    if ip.is_empty() {
        bail!("container {container} has no address on {network}");
    }
    Ok(ip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_lines() {
        let (name, u) = parse_stats_line("demo.1\t0.15%\t1.5MiB / 512MiB").unwrap();
        assert_eq!(name, "demo.1");
        assert!((u.cpu_percent - 0.15).abs() < 1e-9);
        assert_eq!(u.mem_bytes, 1_572_864);
        assert_eq!(parse_docker_size("0B"), Some(0));
        assert_eq!(parse_docker_size("300kB"), Some(300_000));
        assert_eq!(parse_docker_size("lots"), None);
        assert!(parse_stats_line("garbage").is_none());
    }
}

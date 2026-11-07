//! Who an image runs as, and making named volumes writable for that user.

use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::{Context, Result};

use super::Progress;
use super::docker::{docker, docker_bytes, docker_quiet};
use crate::config::{Paths, ServerConfig};
use crate::spec::{ServiceSpec, VolumeSource};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageUser {
    Root,
    Id { uid: u32, gid: u32 },
    /// A user or group name the image's own files do not define.
    Unresolved(String),
}

/// Reads `USER` from the image config and resolves names through the image's
/// own `/etc/passwd` and `/etc/group`.
pub async fn image_user(image: &str) -> Result<ImageUser> {
    let user = docker(&["image", "inspect", "--format", "{{.Config.User}}", image]).await?.trim().to_string();
    let (u, g) = match user.split_once(':') {
        Some((u, g)) => (u.to_string(), Some(g.to_string())),
        None => (user.clone(), None),
    };
    if u.is_empty() || u == "root" || u == "0" {
        return Ok(ImageUser::Root);
    }
    let numeric_u = u.parse::<u32>().ok();
    let numeric_g = g.as_deref().and_then(|g| g.parse::<u32>().ok());
    // Read the account files only when a name has to be resolved.
    let needs_files = numeric_u.is_none() || (g.is_some() && numeric_g.is_none()) || g.is_none();
    let (passwd, group) = if needs_files { read_account_files(image).await.unwrap_or_default() } else { Default::default() };
    let (uid, primary_gid) = match numeric_u {
        Some(uid) => (uid, passwd_by_uid(&passwd, uid)),
        None => match passwd_by_name(&passwd, &u) {
            Some((uid, gid)) => (uid, Some(gid)),
            None => return Ok(ImageUser::Unresolved(user)),
        },
    };
    let gid = match (&g, numeric_g) {
        (_, Some(gid)) => gid,
        (Some(name), None) => match group_by_name(&group, name) {
            Some(gid) => gid,
            None => return Ok(ImageUser::Unresolved(user)),
        },
        (None, _) => primary_gid.unwrap_or(uid),
    };
    if uid == 0 {
        return Ok(ImageUser::Root);
    }
    Ok(ImageUser::Id { uid, gid })
}

async fn read_account_files(image: &str) -> Result<(String, String)> {
    let id = docker(&["create", "--entrypoint", "true", "--label", "mypaas.tmp=1", image]).await?.trim().to_string();
    let passwd = read_file_from(&id, "/etc/passwd").await;
    let group = read_file_from(&id, "/etc/group").await;
    docker_quiet(&["rm", "-f", &id]).await;
    Ok((passwd.unwrap_or_default(), group.unwrap_or_default()))
}

/// `docker cp` writes a tar stream with the one file in it.
async fn read_file_from(container: &str, path: &str) -> Result<String> {
    let tar_bytes = docker_bytes(&["cp", &format!("{container}:{path}"), "-"]).await?;
    let mut archive = tar::Archive::new(&tar_bytes[..]);
    let mut entry = archive.entries()?.next().context("empty archive")??;
    let mut text = String::new();
    entry.by_ref().take(1 << 20).read_to_string(&mut text)?;
    Ok(text)
}

fn passwd_by_name(passwd: &str, name: &str) -> Option<(u32, u32)> {
    passwd.lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        (f.len() >= 4 && f[0] == name).then(|| Some((f[2].parse().ok()?, f[3].parse().ok()?)))?
    })
}

fn passwd_by_uid(passwd: &str, uid: u32) -> Option<u32> {
    passwd.lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        (f.len() >= 4 && f[2].parse() == Ok(uid)).then(|| f[3].parse().ok())?
    })
}

fn group_by_name(group: &str, name: &str) -> Option<u32> {
    group.lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        (f.len() >= 3 && f[0] == name).then(|| f[2].parse().ok())?
    })
}

fn is_empty_dir(dir: &Path) -> Result<bool> {
    Ok(std::fs::read_dir(dir)?.next().is_none())
}

/// Gives empty named volumes to the image's user. A volume that already holds
/// data owned by someone else is left alone with a warning, since a recursive
/// chown of existing data is not something a deploy should do silently.
pub fn prepare_volumes(paths: &Paths, cfg: &ServerConfig, spec: &ServiceSpec, user: &ImageUser, progress: &Progress) -> Result<()> {
    let say = |m: String| {
        let _ = progress.send(m);
    };
    let (uid, gid) = match user {
        ImageUser::Root => return Ok(()),
        ImageUser::Id { uid, gid } => (*uid, *gid),
        ImageUser::Unresolved(u) => {
            if spec.volumes.iter().any(|v| matches!(v.source, VolumeSource::Named(_))) {
                say(format!("warning: cannot resolve the image user {u:?} to a uid; volume ownership is unchanged"));
            }
            return Ok(());
        }
    };
    for v in &spec.volumes {
        let VolumeSource::Named(name) = &v.source else { continue };
        let dir = paths.volumes_dir().join(&spec.service).join(name);
        let meta = std::fs::metadata(&dir).with_context(|| format!("cannot stat {}", dir.display()))?;
        if (meta.uid(), meta.gid()) == (uid, gid) {
            continue;
        }
        if cfg.volumes.chown && is_empty_dir(&dir)? {
            match std::os::unix::fs::chown(&dir, Some(uid), Some(gid)) {
                Ok(()) => say(format!("volume {name}: owned by {uid}:{gid}, the image's user")),
                Err(e) => say(format!("warning: cannot chown volume {name} to {uid}:{gid}: {e}")),
            }
        } else {
            say(format!(
                "warning: volume {name} is owned by {}:{} but the image runs as {uid}:{gid}; if it must be writable, run on the server: chown -R {uid}:{gid} {}",
                meta.uid(),
                meta.gid(),
                dir.display()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWD: &str = "root:x:0:0:root:/root:/bin/sh\npostgres:x:70:70::/var/lib/postgresql:/bin/sh\nnobody:x:65534:65534:nobody:/:/sbin/nologin\n";
    const GROUP: &str = "root:x:0:\npostgres:x:70:\nwww:x:82:\n";

    #[test]
    fn account_lookup() {
        assert_eq!(passwd_by_name(PASSWD, "postgres"), Some((70, 70)));
        assert_eq!(passwd_by_name(PASSWD, "missing"), None);
        assert_eq!(passwd_by_uid(PASSWD, 65534), Some(65534));
        assert_eq!(passwd_by_uid(PASSWD, 1234), None);
        assert_eq!(group_by_name(GROUP, "www"), Some(82));
        assert_eq!(passwd_by_name("garbage\n::\n", "x"), None);
    }

    #[test]
    fn volume_preparation() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { root: dir.path().to_path_buf() };
        let cfg = ServerConfig::default();
        let spec = crate::spec::parse_spec("# mypaas.service=a\n# mypaas.volume=data:/data\n# mypaas.volume=full:/full\n").unwrap();
        for v in ["data", "full"] {
            std::fs::create_dir_all(paths.volumes_dir().join("a").join(v)).unwrap();
        }
        std::fs::write(paths.volumes_dir().join("a/full/file"), "x").unwrap();
        let me = std::fs::metadata(dir.path()).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        // Our own uid: nothing to do.
        prepare_volumes(&paths, &cfg, &spec, &ImageUser::Id { uid: me.uid(), gid: me.gid() }, &tx).unwrap();
        assert!(rx.try_recv().is_err());
        // Another uid: the empty one is chowned (or warns without root), the full one only warns.
        prepare_volumes(&paths, &cfg, &spec, &ImageUser::Id { uid: 65534, gid: 65534 }, &tx).unwrap();
        let lines: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("volume data"));
        assert!(lines[1].contains("chown -R 65534:65534"));
    }
}

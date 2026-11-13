//! Everything that runs on the server: the daemon, deploys and the server CLI.

pub mod api;
pub mod daemon;
pub mod deploy;
pub mod docker;
pub mod guard;
pub mod lan;
pub mod owner;
pub mod setup;
pub mod sockproxy;
pub mod stats;

use std::fs::File;
use std::os::fd::AsRawFd;

use anyhow::{Context, Result};

use crate::config::Paths;

/// Progress lines of a deploy, sent to the client as they happen.
pub type Progress = tokio::sync::mpsc::UnboundedSender<String>;

/// Only one deploy, rollback or removal runs at a time, across the daemon and
/// `mypaas server deploy`. Released when dropped.
pub struct DeployLock {
    _file: File,
}

impl DeployLock {
    pub fn try_acquire(paths: &Paths) -> Result<Option<DeployLock>> {
        let path = paths.root.join("deploy.lock");
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("cannot open {}", path.display()))?;
        // SAFETY: flock on a file descriptor we own.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                return Ok(None);
            }
            return Err(err).context("cannot lock deploy.lock");
        }
        Ok(Some(DeployLock { _file: file }))
    }
}

/// Removes everything in `~/_mypaas/tmp`. Call only while holding the deploy lock.
pub fn clean_tmp(paths: &Paths) -> Result<()> {
    let tmp = paths.tmp();
    std::fs::create_dir_all(&tmp)?;
    for entry in std::fs::read_dir(&tmp)? {
        let p = entry?.path();
        let r = if p.is_dir() { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
        if let Err(e) = r {
            tracing::warn!("cannot remove {}: {e}", p.display());
        }
    }
    Ok(())
}

/// Free bytes available to unprivileged writers in the filesystem holding `path`.
pub fn free_disk(path: &std::path::Path) -> Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: statvfs writes into the zeroed struct; c is a valid C string.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut st) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("statvfs failed");
    }
    Ok(st.f_bavail as u64 * st.f_frsize as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deploy_lock_is_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { root: dir.path().to_path_buf() };
        let a = DeployLock::try_acquire(&paths).unwrap();
        assert!(a.is_some());
        assert!(DeployLock::try_acquire(&paths).unwrap().is_none());
        drop(a);
        assert!(DeployLock::try_acquire(&paths).unwrap().is_some());
    }
}

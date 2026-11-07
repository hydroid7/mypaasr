//! The deploy bundle: a reproducible `tar.zst` of the project, and a strict unpacker.

use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

pub const BUNDLE_DOCKERFILE: &str = "Dockerfile";

#[derive(Debug)]
pub struct PackedFile {
    pub path: String,
    pub size: u64,
}

#[derive(Debug)]
pub struct Packed {
    pub size: u64,
    pub sha256: String,
    pub files: Vec<PackedFile>,
    /// Paths skipped because they are symlinks or special files.
    pub skipped: Vec<String>,
}

impl Packed {
    pub fn total_unpacked(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    pub fn largest(&self, n: usize) -> Vec<&PackedFile> {
        let mut v: Vec<&PackedFile> = self.files.iter().collect();
        v.sort_by(|a, b| b.size.cmp(&a.size).then(a.path.cmp(&b.path)));
        v.truncate(n);
        v
    }
}

/// Relative path, source path, and whether it is a directory.
type Entry = (String, PathBuf, bool);

/// Lists the files to pack: `.gitignore` rules inside the project only, `.git` left out.
/// The chosen Dockerfile is stored as `Dockerfile`.
fn collect(dir: &Path, dockerfile: &Path) -> Result<(Vec<Entry>, Vec<String>)> {
    let dockerfile = std::fs::canonicalize(dockerfile)?;
    let mut entries = Vec::new();
    let mut skipped = Vec::new();
    let walker = ignore::WalkBuilder::new(dir)
        .hidden(false)
        .parents(false)
        .ignore(false)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(false)
        .require_git(false)
        .follow_links(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build();
    for entry in walker {
        let entry = entry?;
        let rel = entry.path().strip_prefix(dir)?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        let rel_str = rel.to_str().with_context(|| format!("path is not UTF-8: {}", rel.display()))?.to_string();
        let ft = entry.file_type().context("unknown file type")?;
        if ft.is_dir() {
            entries.push((rel_str, entry.path().to_path_buf(), true));
        } else if ft.is_file() {
            if rel_str == BUNDLE_DOCKERFILE {
                // Replaced by the chosen Dockerfile below.
                continue;
            }
            entries.push((rel_str, entry.path().to_path_buf(), false));
        } else {
            skipped.push(rel_str);
        }
    }
    entries.push((BUNDLE_DOCKERFILE.to_string(), dockerfile, false));
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok((entries, skipped))
}

/// Packs `dir` into `out` and returns its size and hash. The same tree always
/// gives the same bytes: sorted entries, owner 0, mtime 0, modes 0644/0755.
pub fn pack(dir: &Path, dockerfile: &Path, out: &Path) -> Result<Packed> {
    let (entries, skipped) = collect(dir, dockerfile)?;
    let file = File::create(out).with_context(|| format!("cannot create {}", out.display()))?;
    let hasher = HashingWriter { inner: BufWriter::new(file), hasher: Sha256::new(), count: 0 };
    let encoder = zstd::Encoder::new(hasher, 3)?;
    let mut tar = tar::Builder::new(encoder);
    tar.mode(tar::HeaderMode::Deterministic);
    let mut files = Vec::new();
    for (rel, path, is_dir) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        if is_dir {
            header.set_entry_type(tar::EntryType::Directory);
            header.set_mode(0o755);
            header.set_size(0);
            tar.append_data(&mut header, format!("{rel}/"), io::empty())?;
        } else {
            let f = File::open(&path).with_context(|| format!("cannot read {}", path.display()))?;
            let meta = f.metadata()?;
            let exec = meta.permissions().mode() & 0o111 != 0;
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(if exec { 0o755 } else { 0o644 });
            header.set_size(meta.len());
            tar.append_data(&mut header, &rel, f.take(meta.len()))?;
            files.push(PackedFile { path: rel, size: meta.len() });
        }
    }
    let encoder = tar.into_inner()?;
    let mut hasher = encoder.finish()?;
    hasher.inner.flush()?;
    hasher.inner.get_ref().sync_all()?;
    Ok(Packed { size: hasher.count, sha256: hex::encode(hasher.hasher.finalize()), files, skipped })
}

struct HashingWriter<W> {
    inner: W,
    hasher: Sha256,
    count: u64,
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.count += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct UnpackLimits {
    pub max_unpacked: u64,
    pub max_files: u64,
    pub max_depth: usize,
}

/// Stops reading once more than `limit` bytes have come out of the decompressor.
struct LimitedReader<R> {
    inner: R,
    remaining: u64,
}

impl<R: Read> Read for LimitedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            // Allow detecting a clean end of stream; anything more is over the limit.
            let mut probe = [0u8; 1];
            return match self.inner.read(&mut probe)? {
                0 => Ok(0),
                _ => Err(io::Error::other("bundle is larger than the unpacked size limit")),
            };
        }
        let max = buf.len().min(self.remaining.min(usize::MAX as u64) as usize);
        let n = self.inner.read(&mut buf[..max])?;
        self.remaining -= n as u64;
        Ok(n)
    }
}

/// Unpacks a bundle into `dest`, which must exist and be empty. Only regular files and
/// directories are accepted; anything that could point outside `dest` is refused.
pub fn unpack(bundle: &Path, dest: &Path, limits: UnpackLimits) -> Result<u64> {
    let file = File::open(bundle)?;
    let decoder = zstd::Decoder::new(file)?;
    let reader = LimitedReader { inner: decoder, remaining: limits.max_unpacked };
    let mut archive = tar::Archive::new(reader);
    let mut count = 0u64;
    let mut total = 0u64;
    for entry in archive.entries()? {
        let mut entry = entry.context("corrupt bundle")?;
        count += 1;
        if count > limits.max_files {
            bail!("bundle has more than {} files", limits.max_files);
        }
        let raw_path = entry.path()?.into_owned();
        let rel = checked_relative(&raw_path, limits.max_depth)?;
        let Some(rel) = rel else { continue };
        let target = dest.join(&rel);
        let mode = entry.header().mode().unwrap_or(0o644);
        match entry.header().entry_type() {
            tar::EntryType::Directory => {
                std::fs::create_dir_all(&target)?;
            }
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                // create_new refuses to replace an earlier entry of the same name.
                let mut out = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o644)
                    .open(&target)
                    .with_context(|| format!("duplicate or invalid path {}", rel.display()))?;
                total += io::copy(&mut entry, &mut out)?;
                // Only rwx bits survive; setuid, setgid and sticky are cleared.
                let exec = mode & 0o111 != 0;
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(if exec { 0o755 } else { 0o644 }))?;
            }
            other => bail!("bundle entry {} has unsupported type {:?}", rel.display(), other),
        }
    }
    if !dest.join(BUNDLE_DOCKERFILE).is_file() {
        bail!("bundle has no Dockerfile");
    }
    Ok(total)
}

/// Returns the relative path if it is safe; `None` for the root entry `./`.
fn checked_relative(p: &Path, max_depth: usize) -> Result<Option<PathBuf>> {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Normal(n) => out.push(n),
            Component::CurDir => {}
            _ => bail!("bundle path {} is absolute or contains ..", p.display()),
        }
    }
    if out.components().count() > max_depth {
        bail!("bundle path {} is deeper than {max_depth}", p.display());
    }
    Ok(if out.as_os_str().is_empty() { None } else { Some(out) })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: UnpackLimits = UnpackLimits { max_unpacked: 1 << 20, max_files: 100, max_depth: 8 };

    fn project() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        std::fs::write(p.join("Dockerfile"), "FROM x\n# mypaas.service=a\n").unwrap();
        std::fs::write(p.join("Dockerfile.prod"), "FROM y\n# mypaas.service=a-prod\n").unwrap();
        std::fs::write(p.join(".gitignore"), "target/\n*.log\n").unwrap();
        std::fs::create_dir_all(p.join("src/inner")).unwrap();
        std::fs::write(p.join("src/main.py"), "print(1)").unwrap();
        std::fs::write(p.join("src/inner/x.sh"), "#!/bin/sh").unwrap();
        std::fs::set_permissions(p.join("src/inner/x.sh"), std::fs::Permissions::from_mode(0o4775)).unwrap();
        std::fs::create_dir_all(p.join("target")).unwrap();
        std::fs::write(p.join("target/big"), "x").unwrap();
        std::fs::write(p.join("debug.log"), "x").unwrap();
        std::fs::create_dir_all(p.join(".git")).unwrap();
        std::fs::write(p.join(".git/config"), "x").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", p.join("link")).unwrap();
        d
    }

    #[test]
    fn pack_is_reproducible_and_respects_gitignore() {
        let proj = project();
        let out = tempfile::tempdir().unwrap();
        let a = pack(proj.path(), &proj.path().join("Dockerfile"), &out.path().join("a.tar.zst")).unwrap();
        // Touch a file's mtime and pack again: same hash.
        let f = File::options().write(true).open(proj.path().join("src/main.py")).unwrap();
        f.set_modified(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(12345)).unwrap();
        let b = pack(proj.path(), &proj.path().join("Dockerfile"), &out.path().join("b.tar.zst")).unwrap();
        assert_eq!(a.sha256, b.sha256);
        let names: Vec<&str> = a.files.iter().map(|f| f.path.as_str()).collect();
        assert!(names.contains(&"src/main.py"));
        assert!(!names.iter().any(|n| n.starts_with("target") || n.ends_with(".log") || n.starts_with(".git/")));
        assert_eq!(a.skipped, vec!["link"]);

        let dest = tempfile::tempdir().unwrap();
        unpack(&out.path().join("a.tar.zst"), dest.path(), LIMITS).unwrap();
        assert!(dest.path().join("src/main.py").is_file());
        let mode = std::fs::metadata(dest.path().join("src/inner/x.sh")).unwrap().permissions().mode();
        assert_eq!(mode & 0o7777, 0o755);
    }

    #[test]
    fn chosen_dockerfile_is_stored_as_dockerfile() {
        let proj = project();
        let out = tempfile::tempdir().unwrap();
        pack(proj.path(), &proj.path().join("Dockerfile.prod"), &out.path().join("a.tar.zst")).unwrap();
        let dest = tempfile::tempdir().unwrap();
        unpack(&out.path().join("a.tar.zst"), dest.path(), LIMITS).unwrap();
        let text = std::fs::read_to_string(dest.path().join("Dockerfile")).unwrap();
        assert!(text.contains("a-prod"));
    }

    /// Builds a raw tar.zst with hand-made headers, bypassing the tar crate's path checks.
    fn raw_bundle(entries: &[(&str, tar::EntryType, &[u8])]) -> tempfile::NamedTempFile {
        let mut data = Vec::new();
        for (name, kind, content) in entries {
            let mut h = tar::Header::new_old();
            h.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
            h.set_entry_type(*kind);
            h.set_size(content.len() as u64);
            h.set_mode(0o644);
            if *kind == tar::EntryType::Symlink {
                h.set_link_name("/etc").unwrap();
            }
            h.set_cksum();
            data.extend_from_slice(h.as_bytes());
            data.extend_from_slice(content);
            data.resize(data.len().div_ceil(512) * 512, 0);
        }
        data.resize(data.len() + 1024, 0);
        let f = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(f.path(), zstd::encode_all(&data[..], 3).unwrap()).unwrap();
        f
    }

    fn try_unpack(entries: &[(&str, tar::EntryType, &[u8])], limits: UnpackLimits) -> Result<u64> {
        let b = raw_bundle(entries);
        let dest = tempfile::tempdir().unwrap();
        unpack(b.path(), dest.path(), limits)
    }

    #[test]
    fn rejects_dangerous_entries() {
        use tar::EntryType::*;
        let df: (&str, tar::EntryType, &[u8]) = ("Dockerfile", Regular, b"FROM x");
        assert!(try_unpack(&[df], LIMITS).is_ok());
        assert!(try_unpack(&[df, ("../evil", Regular, b"x")], LIMITS).is_err());
        assert!(try_unpack(&[df, ("/etc/evil", Regular, b"x")], LIMITS).is_err());
        assert!(try_unpack(&[df, ("link", Symlink, b"")], LIMITS).is_err());
        assert!(try_unpack(&[df, ("hard", Link, b"")], LIMITS).is_err());
        assert!(try_unpack(&[df, ("dev", Char, b"")], LIMITS).is_err());
        assert!(try_unpack(&[df, ("Dockerfile", Regular, b"again")], LIMITS).is_err());
        assert!(try_unpack(&[("other", Regular, b"x")], LIMITS).is_err());
        assert!(try_unpack(&[df, ("a/b/c/d/e/f/g/h/i", Regular, b"x")], LIMITS).is_err());
    }

    #[test]
    fn stops_at_limits() {
        use tar::EntryType::*;
        let df: (&str, tar::EntryType, &[u8]) = ("Dockerfile", Regular, b"FROM x");
        let zeros = vec![0u8; 2 << 20];
        let err = try_unpack(&[df, ("bomb", Regular, &zeros)], LIMITS).unwrap_err();
        assert!(format!("{err:#}").contains("limit"));
        let many: Vec<String> = (0..5).map(|i| format!("f{i}")).collect();
        let mut entries = vec![df];
        entries.extend(many.iter().map(|n| (n.as_str(), Regular, &b"x"[..])));
        let small = UnpackLimits { max_files: 3, ..LIMITS };
        assert!(try_unpack(&entries, small).is_err());
    }
}

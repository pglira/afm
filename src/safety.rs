//! Path checks that stop dangerous transfers before rsync starts.
//!
//! The checks use the rsync destination rules: a source without a trailing slash goes to
//! `TARGET/<name>` when TARGET is a directory (it exists as one, ends with `/`, or there is more
//! than one source); otherwise the source goes to TARGET itself. Remote paths are not checked.

use crate::util::is_remote;
use anyhow::{bail, Result};
use std::fs;
use std::path::{Component, Path, PathBuf};

/// Remove `.` and `..` components without file system access.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// Canonical form of a path that possibly does not exist: resolve the longest existing
/// prefix and append the remaining components.
fn resolve(p: &Path) -> PathBuf {
    let p = normalize(p);
    let mut existing = p.clone();
    let mut rest = Vec::new();
    loop {
        if let Ok(mut c) = existing.canonicalize() {
            for part in rest.iter().rev() {
                c.push(part);
            }
            return c;
        }
        match (existing.file_name().map(|n| n.to_os_string()), existing.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                existing = parent.to_path_buf();
            }
            _ => return p,
        }
    }
}

/// Canonical form of a source. A final symlink is not followed, because rsync transfers
/// the link itself; a trailing slash makes rsync follow it.
fn resolve_source(src: &str) -> PathBuf {
    let p = Path::new(src);
    if src.ends_with('/') || src.ends_with("/.") {
        return resolve(p);
    }
    match (p.parent(), p.file_name()) {
        (Some(parent), Some(name)) => resolve(parent).join(name),
        _ => resolve(p),
    }
}

fn is_dir(p: &Path) -> bool {
    fs::metadata(p).map(|m| m.is_dir()).unwrap_or(false)
}

/// Refuse a transfer when:
/// - a source and its destination are the same path,
/// - the destination is inside a source directory,
/// - a source is inside the destination and the rsync options delete files there.
pub fn check(sources: &[String], target: &str, rsync_args: &[String]) -> Result<()> {
    if is_remote(target) {
        return Ok(());
    }
    let t = resolve(Path::new(target));
    let target_is_dir = target.ends_with('/') || sources.len() > 1 || is_dir(&t);
    let deletes = rsync_args.iter().any(|a| a.starts_with("--del"));
    for src in sources {
        if is_remote(src) {
            continue;
        }
        let s = resolve_source(src);
        let dest = match s.file_name() {
            Some(name) if target_is_dir && !src.ends_with('/') && !src.ends_with("/.") => t.join(name),
            _ => t.clone(),
        };
        if dest == s {
            bail!("source and destination are the same path: {}", s.display());
        }
        if is_dir(&s) && dest.starts_with(&s) {
            bail!(
                "the destination {} is inside the source directory {}",
                dest.display(),
                s.display()
            );
        }
        if deletes && s.starts_with(&dest) {
            bail!(
                "the source {} is inside the destination {}, and the --delete option would remove files there",
                s.display(),
                dest.display()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!("afm-test-{}", crate::util::random_hex6()));
            fs::create_dir_all(p.join("a/dir/sub")).unwrap();
            fs::write(p.join("a/dir/f.txt"), "x").unwrap();
            fs::write(p.join("a/file.txt"), "x").unwrap();
            fs::create_dir_all(p.join("b")).unwrap();
            TempDir(p.canonicalize().unwrap())
        }
        fn p(&self, rel: &str) -> String {
            format!("{}/{rel}", self.0.display())
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn ok(d: &TempDir, srcs: &[&str], tgt: &str, args: &[&str]) -> bool {
        let srcs: Vec<String> = srcs.iter().map(|s| d.p(s)).collect();
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        check(&srcs, &d.p(tgt), &args).is_ok()
    }

    #[test]
    fn allows_normal_transfers() {
        let d = TempDir::new();
        assert!(ok(&d, &["a/dir"], "b/", &[]));
        assert!(ok(&d, &["a/dir/"], "b/copy", &[]));
        assert!(ok(&d, &["a/file.txt"], "a/file2.txt", &[]));
        assert!(ok(&d, &["a/dir/"], "a", &[]));
    }

    #[test]
    fn refuses_same_path() {
        let d = TempDir::new();
        assert!(!ok(&d, &["a/dir"], "a/", &[]));
        assert!(!ok(&d, &["a/dir"], "a", &[]));
        assert!(!ok(&d, &["a/dir/"], "a/dir", &[]));
        assert!(!ok(&d, &["a/file.txt"], "a/file.txt", &[]));
        assert!(!ok(&d, &["a/file.txt"], "a/./b/../", &[]));
    }

    #[test]
    fn refuses_destination_inside_source() {
        let d = TempDir::new();
        assert!(!ok(&d, &["a/dir"], "a/dir/sub", &[]));
        assert!(!ok(&d, &["a/dir/"], "a/dir/new", &[]));
        assert!(!ok(&d, &["a"], "a/dir/", &[]));
    }

    #[test]
    fn refuses_delete_over_source() {
        let d = TempDir::new();
        assert!(ok(&d, &["a/dir/"], "a", &[]));
        assert!(!ok(&d, &["a/dir/"], "a", &["--delete"]));
        assert!(!ok(&d, &["a/dir/"], "a", &["--delete-after"]));
        assert!(!ok(&d, &["a/dir/sub/."], "a", &["--delete"]));
    }
}

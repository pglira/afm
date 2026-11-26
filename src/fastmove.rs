//! Fast path of a move: rename a source when it is on the file system of its destination.
//!
//! A rename is used only when it gives the result of the rsync transfer: the paths are
//! local, the job has no extra rsync options, and the destination does not exist. All other
//! sources stay with rsync.

use crate::job::{Job, Op};
use crate::util::is_remote;
use std::ffi::CString;
use std::fs::{self, Metadata};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Totals of the sources that were renamed.
#[derive(Default)]
pub struct Moved {
    /// Size of the regular files.
    pub bytes: u64,
    /// Number of the regular files.
    pub files: u64,
    /// Number of all entries: files, directories, symlinks and special files.
    pub entries: u64,
}

/// True if the job is a move that a rename can do: no dry run, no extra rsync options
/// (they can filter or change the data), and a local target.
pub fn allowed(job: &Job) -> bool {
    job.op == Op::Move && !job.dry_run && job.rsync_args.is_empty() && !is_remote(&job.target)
}

/// Rename each source that a rename can move. Returns the sources that stay for rsync, the
/// totals of the renamed sources, and one log line per renamed source or warning.
pub fn rename_sources(sources: &[String], target: &str) -> (Vec<String>, Moved, Vec<String>) {
    let target_is_dir = target.ends_with('/') || sources.len() > 1 || is_dir(Path::new(target));
    let mut rest = Vec::new();
    let mut moved = Moved::default();
    let mut log = Vec::new();
    // The rename of one source changes the path of a source that overlaps it.
    if overlap(sources) {
        return (sources.to_vec(), moved, log);
    }
    for src in sources {
        match rename_source(src, target, target_is_dir, &mut moved, &mut log) {
            Some(dest) => log.push(format!("afm: renamed {src} → {}", dest.display())),
            None => rest.push(src.clone()),
        }
    }
    (rest, moved, log)
}

/// True if one local source is equal to a different source, or contains it.
fn overlap(sources: &[String]) -> bool {
    let local: Vec<&Path> =
        sources.iter().filter(|s| !is_remote(s)).map(|s| Path::new(s.trim_end_matches('/'))).collect();
    local.iter().enumerate().any(|(i, a)| local.iter().enumerate().any(|(k, b)| i != k && b.starts_with(a)))
}

fn is_dir(p: &Path) -> bool {
    fs::metadata(p).map(|m| m.is_dir()).unwrap_or(false)
}

fn exists(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok()
}

/// Destination of a source by the rsync path rules, and the directory that rsync creates
/// for it when the directory does not exist.
/// - `dir/` (the contents of a directory) goes to TARGET itself.
/// - A directory goes to `TARGET/<name>`; rsync makes TARGET a directory.
/// - A file or a symlink goes to `TARGET/<name>` when TARGET is a directory, else to TARGET.
fn destination(from: &Path, md: &Metadata, contents: bool, target: &Path, target_is_dir: bool) -> Option<(PathBuf, bool)> {
    if !contents && (md.is_dir() || target_is_dir) {
        Some((target.join(from.file_name()?), true))
    } else {
        Some((target.to_path_buf(), false))
    }
}

/// Rename one source to its destination. Returns the destination, or `None` when the
/// source stays for rsync.
fn rename_source(
    src: &str,
    target: &str,
    target_is_dir: bool,
    moved: &mut Moved,
    log: &mut Vec<String>,
) -> Option<PathBuf> {
    if is_remote(src) {
        return None;
    }
    let contents = src.ends_with('/');
    let trimmed = src.trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    let from = Path::new(trimmed);
    let md = fs::symlink_metadata(from).ok()?;
    // rsync follows a symlink that has a trailing slash; a rename moves the link itself.
    if contents && !md.is_dir() {
        return None;
    }
    let target = Path::new(target);
    let (dest, in_target) = destination(from, &md, contents, target, target_is_dir)?;
    if exists(&dest) {
        return None;
    }
    // The device of the destination is the device of its nearest existing directory.
    let parent = dest.parent()?;
    let anchor = if in_target && !exists(parent) { parent.parent()? } else { parent };
    if fs::metadata(anchor).ok()?.dev() != md.dev() {
        return None;
    }
    let size = measure(from, &md, md.dev()).ok()?;
    let created = in_target && !exists(parent);
    if created {
        fs::create_dir(parent).ok()?;
    }
    if rename_noreplace(from, &dest).is_err() {
        if created {
            let _ = fs::remove_dir(parent);
        }
        return None;
    }
    moved.bytes += size.bytes;
    moved.files += size.files;
    moved.entries += size.entries;
    // A source with a trailing slash keeps its top directory.
    if contents {
        if let Err(e) = fs::create_dir(from).and_then(|_| fs::set_permissions(from, md.permissions())) {
            log.push(format!("afm: warning: cannot create the empty source directory {trimmed} again: {e}"));
        }
    }
    Some(dest)
}

/// Count the entries and the file sizes of a tree; symlinks are not followed.
/// A tree with an entry on a different device (a mount point) is an error: a rename
/// moves the mount point, not its data.
fn measure(p: &Path, md: &Metadata, dev: u64) -> io::Result<Moved> {
    if md.dev() != dev {
        return Err(io::Error::from_raw_os_error(libc::EXDEV));
    }
    let mut total = Moved { bytes: 0, files: 0, entries: 1 };
    if md.is_file() {
        total.bytes = md.len();
        total.files = 1;
    } else if md.is_dir() {
        for entry in fs::read_dir(p)? {
            let entry = entry?;
            let sub = measure(&entry.path(), &entry.metadata()?, dev)?;
            total.bytes += sub.bytes;
            total.files += sub.files;
            total.entries += sub.entries;
        }
    }
    Ok(total)
}

/// Rename `from` to `to`; fail when `to` exists.
fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    let c_from = CString::new(from.as_os_str().as_bytes())?;
    let c_to = CString::new(to.as_os_str().as_bytes())?;
    let r = unsafe {
        libc::renameat2(libc::AT_FDCWD, c_from.as_ptr(), libc::AT_FDCWD, c_to.as_ptr(), libc::RENAME_NOREPLACE)
    };
    if r == 0 {
        return Ok(());
    }
    let e = io::Error::last_os_error();
    match e.raw_os_error() {
        // The file system has no atomic "no replace" rename: check, then rename.
        Some(libc::EINVAL | libc::ENOSYS) if !exists(to) => fs::rename(from, to),
        _ => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!("afm-fastmove-{}", crate::util::random_hex6()));
            fs::create_dir_all(p.join("a/dir/sub")).unwrap();
            fs::write(p.join("a/dir/f.txt"), "12345").unwrap();
            fs::write(p.join("a/dir/sub/g.txt"), "123").unwrap();
            fs::write(p.join("a/file.txt"), "12").unwrap();
            fs::create_dir_all(p.join("b")).unwrap();
            TempDir(p.canonicalize().unwrap())
        }
        fn p(&self, rel: &str) -> String {
            format!("{}/{rel}", self.0.display())
        }
        fn has(&self, rel: &str) -> bool {
            exists(Path::new(&self.p(rel)))
        }
        /// Run the renames; return the number of sources that stay for rsync, and the totals.
        fn mv(&self, srcs: &[&str], tgt: &str) -> (usize, Moved) {
            let srcs: Vec<String> = srcs.iter().map(|s| self.p(s)).collect();
            let (rest, moved, _) = rename_sources(&srcs, &self.p(tgt));
            (rest.len(), moved)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn renames_a_directory_into_the_target() {
        let d = TempDir::new();
        let (rest, moved) = d.mv(&["a/dir"], "b");
        assert_eq!(rest, 0);
        assert_eq!((moved.bytes, moved.files, moved.entries), (8, 2, 4));
        assert!(d.has("b/dir/sub/g.txt") && !d.has("a/dir"));
    }

    #[test]
    fn a_directory_makes_a_missing_target_a_directory() {
        let d = TempDir::new();
        assert_eq!(d.mv(&["a/dir"], "new").0, 0);
        assert!(d.has("new/dir/f.txt") && !d.has("a/dir"));
    }

    #[test]
    fn renames_a_file_to_a_new_name_or_into_a_directory() {
        let d = TempDir::new();
        assert_eq!(d.mv(&["a/file.txt"], "b/renamed.txt").0, 0);
        assert!(d.has("b/renamed.txt"));
        assert_eq!(d.mv(&["b/renamed.txt"], "c/").0, 0);
        assert!(d.has("c/renamed.txt"));
    }

    #[test]
    fn contents_go_to_a_new_target_and_the_source_directory_stays() {
        let d = TempDir::new();
        assert_eq!(d.mv(&["a/dir/"], "b/copy").0, 0);
        assert!(d.has("b/copy/f.txt") && d.has("a/dir") && !d.has("a/dir/f.txt"));
    }

    #[test]
    fn an_existing_destination_stays_for_rsync() {
        let d = TempDir::new();
        fs::create_dir_all(d.p("b/dir")).unwrap();
        fs::write(d.p("b/file.txt"), "old").unwrap();
        assert_eq!(d.mv(&["a/dir", "a/file.txt"], "b").0, 2);
        assert_eq!(d.mv(&["a/dir/"], "b").0, 1);
        assert!(d.has("a/dir/f.txt") && d.has("a/file.txt"));
        assert_eq!(fs::read(d.p("b/file.txt")).unwrap(), b"old");
    }

    #[test]
    fn a_destination_inside_the_source_stays_for_rsync() {
        let d = TempDir::new();
        assert_eq!(d.mv(&["a/dir"], "a/dir/sub").0, 1);
        assert!(d.has("a/dir/f.txt"));
        assert_eq!(d.mv(&["a/dir"], "a/dir/new").0, 1);
        assert!(!d.has("a/dir/new"));
    }

    #[test]
    fn sources_that_overlap_stay_for_rsync() {
        let d = TempDir::new();
        assert_eq!(d.mv(&["a/dir", "a"], "c").0, 2);
        assert_eq!(d.mv(&["a/dir/", "a/dir/sub"], "c").0, 2);
        assert_eq!(d.mv(&["a/file.txt", "a/file.txt"], "b").0, 2);
        assert!(d.has("a/dir/sub/g.txt") && d.has("a/file.txt") && !d.has("c"));
    }
}

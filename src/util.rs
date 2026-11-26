use std::collections::hash_map::RandomState;
use std::fs::{self, File};
use std::hash::{BuildHasher, Hasher};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Current time as seconds since the Unix epoch.
pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Random 6-digit hex string.
pub fn random_hex6() -> String {
    let mut h = RandomState::new().build_hasher();
    h.write_u128(SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    h.write_u32(std::process::id());
    format!("{:06x}", h.finish() & 0xff_ffff)
}

/// True if rsync treats the path as remote (`host:path`, `user@host:path`, `rsync://...`).
pub fn is_remote(p: &str) -> bool {
    if p.starts_with("rsync://") {
        return true;
    }
    match p.find(':') {
        Some(i) => i > 0 && !p[..i].contains('/'),
        None => false,
    }
}

/// Make a local path absolute against `cwd`. Remote paths stay as they are.
/// A path that means "the contents of a directory" for rsync (`dir/`, `dir/.`, `.`) gets a
/// trailing slash and no final `.`, so that it keeps this meaning.
pub fn absolutize(p: &str, cwd: &Path) -> String {
    if is_remote(p) {
        return p.to_string();
    }
    let contents = p == "." || p.ends_with("/.") || p.ends_with('/');
    let joined: PathBuf = cwd.join(p).components().collect();
    let mut s = joined.to_string_lossy().into_owned();
    if contents && !s.ends_with('/') {
        s.push('/');
    }
    s
}

/// True if the rsync options make a dry run (`-n`, `--dry-run`).
pub fn is_dry_run(args: &[String]) -> bool {
    args.iter().any(|a| {
        a == "--dry-run" || (a.starts_with('-') && !a.starts_with("--") && a[1..].contains('n'))
    })
}

/// True if the rsync options can leave source directories out of the transfer
/// (filter rules, excludes, file lists, pruning, no recursion).
pub fn has_filters(args: &[String]) -> bool {
    const LONG: &[&str] = &[
        "exclude", "include", "filter", "cvs-exclude", "files-from", "prune-empty-dirs", "no-recursive",
        "no-r", "existing", "ignore-existing", "max-size", "min-size",
    ];
    args.iter().any(|a| {
        if let Some(l) = a.strip_prefix("--") {
            LONG.iter().any(|f| l.starts_with(f))
        } else if let Some(short) = a.strip_prefix('-') {
            short.chars().any(|c| "fFCm".contains(c))
        } else {
            false
        }
    })
}

/// Quote a string for a POSIX shell.
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes().all(|c| c.is_ascii_alphanumeric() || b"-_./=:@,+%".contains(&c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

pub fn shell_join(args: &[String]) -> String {
    args.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ")
}

/// Name of the process that started afm: the first ancestor that is not a shell or wrapper.
pub fn detect_agent() -> Option<String> {
    if let Ok(a) = std::env::var("AFM_AGENT") {
        if !a.is_empty() {
            return Some(a);
        }
    }
    const SKIP: &[&str] = &[
        "sh", "bash", "zsh", "dash", "fish", "ksh", "env", "timeout", "sudo", "nohup", "xargs",
        "script", "afm", "stdbuf", "nice", "time",
    ];
    let mut pid = std::os::unix::process::parent_id();
    for _ in 0..8 {
        if pid <= 1 {
            return None;
        }
        let (comm, ppid) = proc_info(pid)?;
        if !SKIP.contains(&comm.as_str()) {
            return Some(comm);
        }
        pid = ppid;
    }
    None
}

fn proc_info(pid: u32) -> Option<(String, u32)> {
    let s = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let l = s.find('(')?;
    let r = s.rfind(')')?;
    let comm = s[l + 1..r].to_string();
    let ppid = s[r + 1..].split_whitespace().nth(1)?.parse().ok()?;
    Some((comm, ppid))
}

/// State character and start time (in clock ticks after boot) of a process.
pub fn proc_stat(pid: u32) -> Option<(char, u64)> {
    let s = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let r = s.rfind(')')?;
    let fields: Vec<&str> = s[r + 1..].split_whitespace().collect();
    let state = fields.first()?.chars().next()?;
    let start = fields.get(19)?.parse().ok()?;
    Some((state, start))
}

/// True if the process exists, is not a zombie and, when `start` is given, has this start time.
pub fn process_matches(pid: u32, start: Option<u64>) -> bool {
    match proc_stat(pid) {
        Some((state, st)) => state != 'Z' && start.is_none_or(|s| s == st),
        None => false,
    }
}

/// Command name of a process.
pub fn proc_comm(pid: u32) -> Option<String> {
    fs::read_to_string(format!("/proc/{pid}/comm")).ok().map(|s| s.trim().to_string())
}


pub fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if b < 1024 {
        return format!("{b} B");
    }
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", UNITS[i])
}

/// Compact size: `512B`, `1.2K`, `3.4G`.
pub fn short_bytes(b: u64) -> String {
    const UNITS: [&str; 6] = ["B", "K", "M", "G", "T", "P"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b}B")
    } else if v < 10.0 {
        format!("{v:.1}{}", UNITS[i])
    } else {
        format!("{v:.0}{}", UNITS[i])
    }
}

pub fn fmt_duration(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}


/// Local clock time `secs` seconds from now: `14:32`, or `Fri 09:15` on another day.
pub fn eta_clock(secs: u64) -> String {
    let now = chrono::Local::now();
    let end = now + chrono::Duration::seconds(secs.min(i64::MAX as u64 / 2) as i64);
    if end.date_naive() == now.date_naive() {
        end.format("%H:%M").to_string()
    } else {
        end.format("%a %H:%M").to_string()
    }
}

/// Parse an rsync speed such as `41.57MB/s` into bytes per second.
pub fn parse_speed(s: &str) -> f64 {
    let s = s.trim_end_matches("/s");
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == ','))
        .unwrap_or(s.len());
    let num: f64 = s[..split].replace(',', "").parse().unwrap_or(0.0);
    let mult = match s[split..].to_ascii_uppercase().as_str() {
        "KB" => 1024.0,
        "MB" => 1024.0 * 1024.0,
        "GB" => 1024.0 * 1024.0 * 1024.0,
        "TB" => 1024.0f64.powi(4),
        _ => 1.0,
    };
    num * mult
}

/// Parse `h:mm:ss` into seconds.
pub fn parse_hms(s: &str) -> Option<u64> {
    let mut total = 0u64;
    for part in s.split(':') {
        total = total * 60 + part.parse::<u64>().ok()?;
    }
    Some(total)
}

/// Last `n` lines of a file. Reads at most the last 4 MiB.
pub fn tail_lines(path: &Path, n: usize) -> Vec<String> {
    let Ok(mut f) = File::open(path) else {
        return Vec::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(4 * 1024 * 1024);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<&str> = text.lines().collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    let skip = lines.len().saturating_sub(n);
    lines[skip..].iter().map(|s| s.to_string()).collect()
}

/// Meaning of an rsync exit code.
pub fn rsync_exit_meaning(code: i32) -> &'static str {
    match code {
        0 => "success",
        1 => "syntax or usage error",
        2 => "protocol incompatibility",
        3 => "errors selecting input/output files, dirs",
        4 => "requested action not supported",
        5 => "error starting client-server protocol",
        10 => "error in socket I/O",
        11 => "error in file I/O",
        12 => "error in rsync protocol data stream",
        13 => "errors with program diagnostics",
        14 => "error in IPC code",
        20 => "received SIGUSR1 or SIGINT",
        21 => "some error returned by waitpid()",
        22 => "error allocating core memory buffers",
        23 => "partial transfer due to error",
        24 => "partial transfer due to vanished source files",
        25 => "the --max-delete limit stopped deletions",
        30 => "timeout in data send/receive",
        35 => "timeout waiting for daemon connection",
        255 => "ssh connection failed",
        _ => "unknown error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolutize_keeps_rsync_meaning() {
        let cwd = Path::new("/w/src");
        assert_eq!(absolutize(".", cwd), "/w/src/");
        assert_eq!(absolutize("./", cwd), "/w/src/");
        assert_eq!(absolutize("sub/.", cwd), "/w/src/sub/");
        assert_eq!(absolutize("sub/", cwd), "/w/src/sub/");
        assert_eq!(absolutize("sub", cwd), "/w/src/sub");
        assert_eq!(absolutize("/a/b/.", cwd), "/a/b/");
        assert_eq!(absolutize("/.", cwd), "/");
        assert_eq!(absolutize("host:x/.", cwd), "host:x/.");
    }

    #[test]
    fn detects_dry_run_and_filters() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(is_dry_run(&v(&["--dry-run"])));
        assert!(is_dry_run(&v(&["-vn"])));
        assert!(!is_dry_run(&v(&["--no-perms"])));
        assert!(has_filters(&v(&["--exclude", "cache/"])));
        assert!(has_filters(&v(&["--exclude=cache/"])));
        assert!(has_filters(&v(&["-m"])));
        assert!(!has_filters(&v(&["--bwlimit=5M", "-z"])));
    }
}

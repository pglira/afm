//! The worker process of a job: runs rsync, parses its progress and writes `job.json`.

use crate::fastmove::{self, Moved};
use crate::job::{log_path, take_control, Job, Op, Status};
use crate::util::{fmt_duration, has_filters, human_bytes, is_remote, now, proc_stat, parse_hms, parse_speed, rsync_exit_meaning, shell_join, shell_quote};
use anyhow::Result;
use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{self, BufWriter, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

pub const SSH_COMMAND: &str = "ssh -o BatchMode=yes";

enum Msg {
    Out(String),
    Err(String),
}

struct Progress {
    bytes: u64,
    percent: u8,
    speed: f64,
    /// Remaining time on interim lines; elapsed time on lines that end a file (`xfr#`).
    time: u64,
    end_of_file: bool,
    /// Number of transferred files from `xfr#N`.
    transferred: Option<u64>,
    /// (remaining, total) from `to-chk=` or `ir-chk=`.
    check: Option<(u64, u64)>,
}

/// Parse one `--info=progress2` line, such as
/// `  31,457,280  33%   41.57MB/s    0:00:02 (xfr#2, to-chk=2/6)`.
fn parse_progress(s: &str) -> Option<Progress> {
    let mut it = s.split_whitespace();
    let b = it.next()?;
    if !b.bytes().all(|c| c.is_ascii_digit() || c == b',') {
        return None;
    }
    let bytes = b.replace(',', "").parse().ok()?;
    let percent = it.next()?.strip_suffix('%')?.parse().ok()?;
    let sp = it.next()?;
    if !sp.ends_with("/s") {
        return None;
    }
    let time = parse_hms(it.next()?)?;
    let rest = it.collect::<Vec<_>>().join(" ");
    let check = rest.find("-chk=").and_then(|i| {
        let r = &rest[i + 5..];
        let r = &r[..r.find(')').unwrap_or(r.len())];
        let (a, b) = r.split_once('/')?;
        Some((a.parse().ok()?, b.parse().ok()?))
    });
    let transferred = rest.find("xfr#").and_then(|i| {
        let r = &rest[i + 4..];
        r[..r.find(|c: char| !c.is_ascii_digit()).unwrap_or(r.len())].parse().ok()
    });
    Some(Progress {
        transferred,
        bytes,
        percent,
        speed: parse_speed(sp),
        time,
        end_of_file: rest.contains("xfr#"),
        check,
    })
}

/// Split a byte stream on `\r` and `\n` and call `f` for each non-empty segment.
fn read_segments(mut r: impl Read, mut f: impl FnMut(String)) {
    let mut buf = [0u8; 65536];
    let mut cur = Vec::new();
    loop {
        let n = match r.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        for &c in &buf[..n] {
            if c == b'\r' || c == b'\n' {
                if !cur.is_empty() {
                    f(String::from_utf8_lossy(&cur).into_owned());
                    cur.clear();
                }
            } else {
                cur.push(c);
            }
        }
    }
    if !cur.is_empty() {
        f(String::from_utf8_lossy(&cur).into_owned());
    }
}

fn killpg(pgid: i32, sig: i32) {
    unsafe {
        libc::kill(-pgid, sig);
    }
}

pub fn run(id: &str) -> Result<()> {
    let mut job = Job::load_raw(id)?;
    let pid = std::process::id();
    job.worker_pid = Some(pid);
    job.worker_start = proc_stat(pid).map(|(_, start)| start);
    job.status = Status::Running;
    job.started = Some(now());
    job.save()?;

    // Log and state writes in the loop ignore errors: a full disk must not stop the
    // supervision of a running rsync.
    let mut log = BufWriter::new(OpenOptions::new().create(true).append(true).open(log_path(id))?);

    // Sources on the file system of their destination are renamed; rsync moves the rest.
    let mut command = job.command.clone();
    let mut renamed = Moved::default();
    if fastmove::allowed(&job) {
        let (rest, moved, lines) = fastmove::rename_sources(&job.sources, &job.target);
        for l in &lines {
            let _ = writeln!(log, "{l}");
        }
        renamed = moved;
        job.bytes = renamed.bytes;
        job.files_transferred = renamed.files;
        if rest.is_empty() {
            job.status = Status::Done;
            job.percent = 100;
            job.eta = Some(0);
            job.files_total = Some(renamed.entries);
            job.files_done = renamed.entries;
            return finish(&mut job, &mut log, "all sources renamed");
        }
        // The rsync argv ends with the sources and the target.
        command.truncate(command.len().saturating_sub(job.sources.len() + 1));
        command.extend(rest);
        command.push(job.target.clone());
    }
    let _ = writeln!(log, "$ {}", shell_join(&command));
    let _ = log.flush();

    // A rename can move the working directory of the job; the paths of the job are absolute.
    let cwd = if Path::new(&job.cwd).is_dir() { job.cwd.as_str() } else { "/" };
    let mut cmd = Command::new(&command[0]);
    cmd.args(&command[1..])
        .current_dir(cwd)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    // rsync stops when the worker dies, so that no transfer runs without supervision.
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    if std::env::var_os("RSYNC_RSH").is_none() {
        cmd.env("RSYNC_RSH", SSH_COMMAND);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            job.status = Status::Failed;
            job.error = Some(format!("cannot start rsync: {e}"));
            return finish(&mut job, &mut log, "rsync did not start");
        }
    };
    let pgid = child.id() as i32;
    job.rsync_pid = Some(child.id());
    let _ = job.save();

    let (tx, rx) = mpsc::channel::<Msg>();
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let tx_out = tx.clone();
    thread::spawn(move || read_segments(stdout, |s| drop(tx_out.send(Msg::Out(s)))));
    thread::spawn(move || read_segments(stderr, |s| drop(tx.send(Msg::Err(s)))));

    let mut errors: VecDeque<String> = VecDeque::new();
    let mut cancel_at: Option<Instant> = None;
    let mut killed = false;
    let mut exit_status = None;
    let mut drain_until: Option<Instant> = None;
    let mut dirty = false;
    let mut last_save = Instant::now();
    let mut last_check = Instant::now();
    let tick = Duration::from_millis(200);

    loop {
        match rx.recv_timeout(tick) {
            Ok(Msg::Out(s)) => {
                dirty = true;
                if let Some(p) = parse_progress(&s) {
                    job.bytes = renamed.bytes + p.bytes;
                    job.percent = total_percent(renamed.bytes, p.bytes, p.percent);
                    if job.status == Status::Running {
                        job.speed = p.speed;
                        if p.percent >= 100 {
                            job.eta = Some(0);
                        } else if !p.end_of_file {
                            job.eta = if p.percent == 0 && p.time == 0 { None } else { Some(p.time) };
                        }
                    }
                    if let Some(n) = p.transferred {
                        job.files_transferred = renamed.files + n;
                    }
                    if let Some((remaining, total)) = p.check {
                        job.files_total = Some(renamed.entries + total);
                        job.files_done = renamed.entries + total.saturating_sub(remaining);
                    }
                } else {
                    let _ = writeln!(log, "{s}");
                    job.current_file = Some(s);
                }
            }
            Ok(Msg::Err(s)) => {
                dirty = true;
                let _ = writeln!(log, "{s}");
                errors.push_back(s);
                if errors.len() > 5 {
                    errors.pop_front();
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        if last_check.elapsed() >= tick {
            last_check = Instant::now();

            // A process that inherits the output pipes (for example an ssh master) can keep
            // them open after rsync exits. Stop to read them a short time after the exit.
            if exit_status.is_none() {
                if let Ok(Some(st)) = child.try_wait() {
                    exit_status = Some(st);
                    drain_until = Some(Instant::now() + Duration::from_secs(2));
                }
            }
            if drain_until.is_some_and(|d| Instant::now() >= d) {
                break;
            }

            if exit_status.is_none() {
                if let Some(at) = cancel_at {
                    if !killed && at.elapsed() >= Duration::from_secs(10) {
                        let _ = writeln!(log, "afm: rsync did not stop after SIGTERM; sending SIGKILL");
                        killpg(pgid, libc::SIGKILL);
                        killed = true;
                    }
                }
                if let Some(c) = take_control(id) {
                    match c.as_str() {
                        "cancel" if cancel_at.is_none() => {
                            cancel_at = Some(Instant::now());
                            let _ = writeln!(log, "afm: cancel requested");
                            killpg(pgid, libc::SIGTERM);
                            killpg(pgid, libc::SIGCONT);
                        }
                        "pause" if job.status == Status::Running => {
                            killpg(pgid, libc::SIGSTOP);
                            job.status = Status::Paused;
                            job.speed = 0.0;
                            job.eta = None;
                            dirty = true;
                            last_save = Instant::now() - Duration::from_secs(1);
                        }
                        "resume" if job.status == Status::Paused => {
                            killpg(pgid, libc::SIGCONT);
                            job.status = Status::Running;
                            dirty = true;
                            last_save = Instant::now() - Duration::from_secs(1);
                        }
                        _ => {}
                    }
                }
            }
        }

        if dirty && last_save.elapsed() >= Duration::from_millis(300) {
            let _ = log.flush();
            let _ = job.save();
            dirty = false;
            last_save = Instant::now();
        }
    }

    let status = match exit_status {
        Some(s) => s,
        None => child.wait()?,
    };
    job.exit_code = status.code();
    job.speed = 0.0;
    job.rsync_pid = None;
    let last_errors = errors.iter().cloned().collect::<Vec<_>>().join("\n");

    if cancel_at.is_some() {
        job.status = Status::Cancelled;
        job.eta = None;
    } else {
        match status.code() {
            Some(code @ (0 | 24)) => {
                let mut warnings = Vec::new();
                if code == 24 {
                    warnings.push("warning: some source files vanished during the transfer".to_string());
                }
                if job.op == Op::Move && !job.dry_run {
                    if has_filters(&job.rsync_args) {
                        warnings.push(
                            "note: empty source directories were kept, because filter options are in use".to_string(),
                        );
                    } else {
                        warnings.extend(remove_empty_source_dirs(&job.sources));
                    }
                }
                job.status = Status::Done;
                job.percent = 100;
                job.eta = Some(0);
                if let Some(total) = job.files_total {
                    job.files_done = total;
                }
                if !warnings.is_empty() {
                    job.error = Some(warnings.join("\n"));
                }
            }
            Some(code) => {
                job.status = Status::Failed;
                job.eta = None;
                job.error = Some(format!(
                    "rsync exit code {code} ({}){}{}",
                    rsync_exit_meaning(code),
                    if last_errors.is_empty() { "" } else { ":\n" },
                    last_errors
                ));
            }
            None => {
                job.status = Status::Failed;
                job.eta = None;
                job.error = Some("rsync was killed by a signal".into());
            }
        }
    }
    let detail = job.exit_code.map(|c| format!("rsync exit code {c}")).unwrap_or_else(|| "no exit code".into());
    finish(&mut job, &mut log, &detail)
}

/// Percent of the full job: rsync reports `bytes` and `percent` only for its own part,
/// after `renamed` bytes were moved by a rename.
fn total_percent(renamed: u64, bytes: u64, percent: u8) -> u8 {
    if renamed == 0 || percent == 0 {
        return percent;
    }
    let rsync_total = bytes as u128 * 100 / percent as u128;
    let done = renamed as u128 + bytes as u128;
    (done * 100 / (renamed as u128 + rsync_total).max(1)).min(100) as u8
}

/// Write the end state of the job to the log and to `job.json`, and send the notification.
fn finish(job: &mut Job, log: &mut impl Write, detail: &str) -> Result<()> {
    job.finished = Some(now());
    let _ = writeln!(log, "afm: job {} ({detail})", job.status.as_str());
    if let Some(e) = &job.error {
        let _ = writeln!(log, "afm: {e}");
    }
    let _ = log.flush();
    job.save()?;
    notify(job);
    Ok(())
}

/// Send a desktop notification with `notify-send` for a failed job, and for a done job
/// that ran at least `$AFM_NOTIFY_MIN_SECS` seconds (default 10). `AFM_NOTIFY=0` turns
/// notifications off.
fn notify(job: &Job) {
    if std::env::var("AFM_NOTIFY").is_ok_and(|v| v == "0") {
        return;
    }
    let min_secs = std::env::var("AFM_NOTIFY_MIN_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(10);
    let elapsed = job.elapsed().unwrap_or(0);
    let name = job.name.clone().unwrap_or_else(|| job.id.clone());
    let (title, body, urgency, icon) = match job.status {
        Status::Done if elapsed >= min_secs => (
            format!("{name}: done{}", if job.dry_run { " (dry run)" } else { "" }),
            format!(
                "{} {} in {}\n{}",
                job.op.as_str(),
                human_bytes(job.bytes),
                fmt_duration(elapsed),
                job.route()
            ),
            "normal",
            "emblem-default",
        ),
        Status::Failed => (
            format!("{name}: failed"),
            format!(
                "{}\nafm log {}",
                job.error.as_deref().and_then(|e| e.lines().next()).unwrap_or("unknown error"),
                job.id
            ),
            "critical",
            "dialog-error",
        ),
        _ => return,
    };
    let _ = Command::new("notify-send")
        .args(["-a", "afm", "-u", urgency, "-i", icon, &title, &body])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Remove the empty directories that a move leaves in its sources.
/// A source with a trailing slash keeps its top directory. Returns the warnings.
fn remove_empty_source_dirs(sources: &[String]) -> Vec<String> {
    let mut warnings = Vec::new();
    for src in sources {
        let keep_top = src.ends_with('/');
        if src.starts_with("rsync://") {
            continue;
        }
        if is_remote(src) {
            let (host, path) = src.split_once(':').unwrap_or((src, ""));
            let path = path.strip_prefix("~/").unwrap_or(path);
            let path = if path.is_empty() { "." } else { path };
            let script = format!(
                "find {} {}-type d -empty -delete",
                shell_quote(path),
                if keep_top { "-mindepth 1 " } else { "" }
            );
            let ok = Command::new("ssh")
                .args(["-o", "BatchMode=yes", host, &script])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !ok {
                warnings.push(format!("warning: cannot remove empty source directories in {src}"));
            }
        } else if let Err(e) = remove_empty_dirs(Path::new(src), !keep_top) {
            warnings.push(format!("warning: cannot remove empty source directories in {src}: {e}"));
        }
    }
    warnings
}

fn remove_empty_dirs(p: &Path, remove_self: bool) -> io::Result<()> {
    let md = match fs::symlink_metadata(p) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if !md.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(p)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            remove_empty_dirs(&entry.path(), true)?;
        }
    }
    if remove_self {
        match fs::remove_dir(p) {
            Ok(()) => {}
            Err(e) if e.raw_os_error() == Some(libc::ENOTEMPTY) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_interim_line() {
        let p = parse_progress("     43,188,224  45%   40.04MB/s    0:00:01  ").unwrap();
        assert_eq!(p.bytes, 43_188_224);
        assert_eq!(p.percent, 45);
        assert_eq!(p.time, 1);
        assert!(!p.end_of_file);
        assert!(p.check.is_none());
    }

    #[test]
    fn parses_end_of_file_line() {
        let p = parse_progress("   94,371,840 100%   40.02MB/s    0:00:02 (xfr#4, to-chk=0/6)").unwrap();
        assert_eq!(p.percent, 100);
        assert!(p.end_of_file);
        assert_eq!(p.check, Some((0, 6)));
        assert_eq!(p.transferred, Some(4));
    }

    #[test]
    fn total_percent_includes_the_renamed_bytes() {
        assert_eq!(total_percent(0, 50, 50), 50);
        assert_eq!(total_percent(100, 0, 0), 0);
        assert_eq!(total_percent(100, 50, 50), 75);
        assert_eq!(total_percent(300, 100, 100), 100);
    }

    #[test]
    fn rejects_file_names() {
        assert!(parse_progress("src/sub/f1").is_none());
        assert!(parse_progress("2024 report.pdf").is_none());
    }
}

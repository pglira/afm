//! Job model and the on-disk job store.
//!
//! Each job lives in `<state dir>/jobs/<id>/`:
//! - `job.json`  job state; only the worker process of the job writes it while the job is active
//! - `rsync.log` rsync output (file names and errors)
//! - `control`   one command (`pause`, `resume`, `cancel`) for the worker; the worker removes it

use crate::util::{now, process_matches};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Starting,
    Running,
    Paused,
    Done,
    Failed,
    Cancelled,
    /// The worker process stopped without a final status.
    Lost,
}

impl Status {
    pub fn is_final(self) -> bool {
        matches!(self, Status::Done | Status::Failed | Status::Cancelled | Status::Lost)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Status::Starting => "starting",
            Status::Running => "running",
            Status::Paused => "paused",
            Status::Done => "done",
            Status::Failed => "failed",
            Status::Cancelled => "cancelled",
            Status::Lost => "lost",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    Copy,
    Move,
}

impl Op {
    pub fn as_str(self) -> &'static str {
        match self {
            Op::Copy => "copy",
            Op::Move => "move",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Job {
    pub id: String,
    pub op: Op,
    pub sources: Vec<String>,
    pub target: String,
    /// Short name of the operation.
    #[serde(default, alias = "label")]
    pub name: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    pub cwd: String,
    /// Full rsync argv.
    pub command: Vec<String>,
    pub created: f64,
    #[serde(default)]
    pub started: Option<f64>,
    #[serde(default)]
    pub finished: Option<f64>,
    pub status: Status,
    /// Bytes processed so far, as rsync reports them.
    #[serde(default)]
    pub bytes: u64,
    #[serde(default)]
    pub percent: u8,
    /// Bytes per second.
    #[serde(default)]
    pub speed: f64,
    /// Estimated seconds to completion.
    #[serde(default)]
    pub eta: Option<u64>,
    #[serde(default)]
    pub files_done: u64,
    #[serde(default)]
    pub files_total: Option<u64>,
    #[serde(default)]
    pub current_file: Option<String>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    /// Error text for a failed job, or a warning for a done job.
    #[serde(default)]
    pub error: Option<String>,
    /// Extra rsync options that the caller gave.
    #[serde(default)]
    pub rsync_args: Vec<String>,
    /// The rsync options make a dry run; nothing changes.
    #[serde(default)]
    pub dry_run: bool,
    /// Files that rsync transferred (`xfr#`).
    #[serde(default)]
    pub files_transferred: u64,
    #[serde(default)]
    pub worker_pid: Option<u32>,
    /// Start time of the worker process, to detect a reused PID.
    #[serde(default)]
    pub worker_start: Option<u64>,
    #[serde(default)]
    pub rsync_pid: Option<u32>,
}

impl Job {
    pub fn save(&self) -> Result<()> {
        let dir = job_dir(&self.id);
        fs::create_dir_all(&dir)?;
        let tmp = dir.join(format!(".job.json.{}.tmp", std::process::id()));
        fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        fs::rename(&tmp, dir.join("job.json"))?;
        Ok(())
    }

    /// Load the job as stored, without the liveness check.
    pub fn load_raw(id: &str) -> Result<Job> {
        let path = job_dir(id).join("job.json");
        let data = fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?;
        Ok(serde_json::from_slice(&data)?)
    }

    /// Load the job and mark it `Lost` if its worker process no longer exists.
    pub fn load(id: &str) -> Result<Job> {
        let mut job = Self::load_raw(id)?;
        if !job.status.is_final() {
            let dead = match job.worker_pid {
                Some(pid) => !process_matches(pid, job.worker_start),
                None => now() - job.created > 15.0,
            };
            if dead {
                job.status = Status::Lost;
            }
        }
        Ok(job)
    }

    /// Estimated total size of the transfer.
    pub fn total_estimate(&self) -> Option<u64> {
        if self.status == Status::Done {
            Some(self.bytes)
        } else if self.percent > 0 {
            Some(self.bytes * 100 / self.percent as u64)
        } else {
            None
        }
    }

    pub fn elapsed(&self) -> Option<u64> {
        let start = self.started?;
        let end = self.finished.unwrap_or_else(now);
        Some((end - start).max(0.0) as u64)
    }

    /// `src -> target`, with a count for more than one source.
    pub fn route(&self) -> String {
        let src = match self.sources.len() {
            0 => String::new(),
            1 => self.sources[0].clone(),
            n => format!("{} (+{})", self.sources[0], n - 1),
        };
        format!("{src} → {}", self.target)
    }
}

pub fn state_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("AFM_STATE_DIR").filter(|d| !d.is_empty()) {
        return d.into();
    }
    if let Some(d) = std::env::var_os("XDG_STATE_HOME").filter(|d| !d.is_empty()) {
        return PathBuf::from(d).join("afm");
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/tmp".into());
    home.join(".local/state/afm")
}

pub fn jobs_dir() -> PathBuf {
    state_dir().join("jobs")
}

pub fn job_dir(id: &str) -> PathBuf {
    jobs_dir().join(id)
}

pub fn log_path(id: &str) -> PathBuf {
    job_dir(id).join("rsync.log")
}

fn control_path(id: &str) -> PathBuf {
    job_dir(id).join("control")
}

/// All jobs: active jobs first, then newest first.
pub fn list_jobs() -> Vec<Job> {
    let Ok(rd) = fs::read_dir(jobs_dir()) else {
        return Vec::new();
    };
    let mut jobs: Vec<Job> = rd
        .flatten()
        .filter_map(|e| Job::load(&e.file_name().to_string_lossy()).ok())
        .collect();
    jobs.sort_by(|a, b| {
        a.status
            .is_final()
            .cmp(&b.status.is_final())
            .then(b.created.partial_cmp(&a.created).unwrap_or(std::cmp::Ordering::Equal))
    });
    jobs
}

/// Resolve a full job id or a unique prefix of one.
pub fn resolve_id(prefix: &str) -> Result<String> {
    if !prefix.is_empty() && job_dir(prefix).join("job.json").exists() {
        return Ok(prefix.to_string());
    }
    let matches: Vec<String> = fs::read_dir(jobs_dir())
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with(prefix))
                .collect()
        })
        .unwrap_or_default();
    match matches.len() {
        1 => Ok(matches.into_iter().next().unwrap()),
        0 => bail!("no job with id '{prefix}'"),
        _ => bail!("job id '{prefix}' is ambiguous: {}", matches.join(", ")),
    }
}

pub fn send_control(id: &str, cmd: &str) -> Result<()> {
    let dir = job_dir(id);
    let tmp = dir.join(format!(".control.{}.tmp", std::process::id()));
    fs::write(&tmp, cmd)?;
    fs::rename(&tmp, control_path(id))?;
    Ok(())
}

/// Read and remove the pending control command of a job.
pub fn take_control(id: &str) -> Option<String> {
    let path = control_path(id);
    let cmd = fs::read_to_string(&path).ok()?;
    let _ = fs::remove_file(&path);
    Some(cmd.trim().to_string())
}

pub fn remove_job(id: &str) -> Result<()> {
    fs::remove_dir_all(job_dir(id))?;
    Ok(())
}

/// Remove all finished jobs. Returns the number of removed jobs.
pub fn clear_finished() -> usize {
    list_jobs()
        .into_iter()
        .filter(|j| j.status.is_final())
        .filter(|j| remove_job(&j.id).is_ok())
        .count()
}

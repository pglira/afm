mod dash;
mod job;
mod safety;
mod util;
mod worker;

use anyhow::{bail, Result};
use clap::{Args, Parser, Subcommand};
use job::{list_jobs, log_path, resolve_id, send_control, Job, Op, Status};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};
use util::*;

/// Skill file (SKILL.md) that teaches an agent how to use afm.
const SKILL: &str = include_str!("../SKILL.md");
/// Prompt that tells an agent to install the skill.
pub const INSTALL_PROMPT: &str = include_str!("../INSTALL_PROMPT.md");

/// Exit code of `afm wait` when the timeout ends before the job.
const EXIT_TIMEOUT: u8 = 124;

#[derive(Parser)]
#[command(name = "afm", version, about = "Copy and move files with rsync and watch the jobs in a dashboard")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Copy SOURCE... to TARGET
    Cp(Transfer),
    /// Move SOURCE... to TARGET (each source file is removed after its transfer)
    Mv(Transfer),
    /// Show one job, or all jobs
    #[command(alias = "ls")]
    Status {
        id: Option<String>,
        /// Print JSON
        #[arg(long)]
        json: bool,
    },
    /// Wait until a job ends
    Wait {
        id: String,
        /// Stop waiting after SECONDS and exit with code 124
        #[arg(short, long, value_name = "SECONDS")]
        timeout: Option<u64>,
    },
    /// Show the rsync log of a job
    Log {
        id: String,
        /// Number of lines
        #[arg(short = 'n', long, default_value_t = 50)]
        lines: usize,
    },
    /// Cancel a job
    Cancel { id: String },
    /// Pause a job
    Pause { id: String },
    /// Resume a paused job
    Resume { id: String },
    /// Remove all finished jobs from the list
    Clear,
    /// Open the dashboard
    Dash,
    /// Print the agent skill file (SKILL.md)
    Skill,
    /// Print the prompt that tells an agent to install the skill
    #[command(alias = "agent-help")]
    InstallPrompt,
    #[command(name = "__run", hide = true)]
    Run { id: String },
}

#[derive(Args)]
struct Transfer {
    /// Return at once, print the job id and keep the job running in the background
    #[arg(short, long)]
    detach: bool,
    /// Short name of the operation, shown in the dashboard
    #[arg(short, long, visible_alias = "label")]
    name: String,
    /// Name of the agent (default: $AFM_AGENT, else the name of the parent process)
    #[arg(long)]
    agent: Option<String>,
    /// Exact copy: also keep hard links, ACLs and extended attributes (rsync -HAX)
    #[arg(short = 'x', long)]
    exact: bool,
    /// Skip the path safety checks (same path, target inside source, --delete over source)
    #[arg(long)]
    force: bool,
    /// SOURCE... TARGET (rsync path rules: a trailing slash on SOURCE copies the contents)
    #[arg(required = true, num_args = 2.., value_name = "PATH")]
    paths: Vec<String>,
    /// Extra rsync options, after `--`
    #[arg(last = true, value_name = "RSYNC_OPTION")]
    rsync_args: Vec<String>,
}

fn main() -> ExitCode {
    // Exit quietly when the reader of stdout closes the pipe, for example `afm status | head`.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();
    match run(cli.cmd) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("afm: error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cmd: Cmd) -> Result<ExitCode> {
    match cmd {
        Cmd::Cp(t) => submit(Op::Copy, t),
        Cmd::Mv(t) => submit(Op::Move, t),
        Cmd::Status { id, json } => status(id, json),
        Cmd::Wait { id, timeout } => wait(&resolve_id(&id)?, timeout, None),
        Cmd::Log { id, lines } => {
            let id = resolve_id(&id)?;
            for l in tail_lines(&log_path(&id), lines) {
                println!("{l}");
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Cancel { id } => cancel(&resolve_id(&id)?),
        Cmd::Pause { id } => control(&resolve_id(&id)?, "pause", Status::Running),
        Cmd::Resume { id } => control(&resolve_id(&id)?, "resume", Status::Paused),
        Cmd::Clear => {
            println!("removed {} finished job(s)", job::clear_finished());
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Dash => {
            dash::run()?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Skill => {
            print!("{SKILL}");
            Ok(ExitCode::SUCCESS)
        }
        Cmd::InstallPrompt => {
            print!("{INSTALL_PROMPT}");
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Run { id } => {
            worker::run(&id)?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn submit(op: Op, t: Transfer) -> Result<ExitCode> {
    let cwd = std::env::current_dir()?;
    let mut paths = t.paths;
    let target = paths.pop().expect("clap requires two paths");
    let sources = paths;

    for s in &sources {
        if !is_remote(s) && std::fs::symlink_metadata(s).is_err() {
            bail!("source does not exist: {s}");
        }
    }
    if is_remote(&target) && sources.iter().any(|s| is_remote(s)) {
        bail!("rsync cannot transfer between two remote hosts");
    }
    let sources: Vec<String> = sources.iter().map(|s| absolutize(s, &cwd)).collect();
    let target = absolutize(&target, &cwd);
    if !t.force {
        safety::check(&sources, &target, &t.rsync_args)?;
    }
    let dry_run = is_dry_run(&t.rsync_args);
    let extra_args = t.rsync_args.clone();

    let mut command: Vec<String> = [
        "rsync",
        "-a",
        "--partial",
        "--info=progress2,name1",
        "--no-inc-recursive",
        "--outbuf=L",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if t.exact {
        command.push("-HAX".into());
    }
    if op == Op::Move {
        command.push("--remove-source-files".into());
    }
    command.extend(t.rsync_args);
    command.extend(sources.iter().cloned());
    command.push(target.clone());

    std::fs::create_dir_all(job::jobs_dir())?;
    let id = loop {
        let id = random_hex6();
        if std::fs::create_dir(job::job_dir(&id)).is_ok() {
            break id;
        }
    };
    let job = Job {
        id: id.clone(),
        op,
        sources,
        target,
        name: Some(t.name),
        rsync_args: extra_args,
        dry_run,
        files_transferred: 0,
        worker_start: None,
        agent: t.agent.or_else(detect_agent),
        cwd: cwd.to_string_lossy().into_owned(),
        command,
        created: now(),
        started: None,
        finished: None,
        status: Status::Starting,
        bytes: 0,
        percent: 0,
        speed: 0.0,
        eta: None,
        files_done: 0,
        files_total: None,
        current_file: None,
        exit_code: None,
        error: None,
        worker_pid: None,
        rsync_pid: None,
    };
    job.save()?;

    let mut worker = Command::new(std::env::current_exe()?);
    worker
        .args(["__run", &id])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        worker.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let child = worker.spawn()?;

    println!("job {id} started: {} {}", op.as_str(), job.route());
    if t.detach {
        println!("Wait with: afm wait {id} --timeout 500    Status: afm status {id}");
        return Ok(ExitCode::SUCCESS);
    }
    let _ = std::io::stdout().flush();
    wait(&id, None, Some(child))
}

fn progress_line(j: &Job) -> String {
    let total = j.total_estimate().map(|t| format!(" of ~{}", human_bytes(t))).unwrap_or_default();
    let eta = j.eta.map(|e| format!("{} (ends {})", fmt_duration(e), eta_clock(e))).unwrap_or_else(|| "?".into());
    format!(
        "job {} {}: {}% ({}{}), {}/s, ETA {}",
        j.id,
        j.status.as_str(),
        j.percent,
        human_bytes(j.bytes),
        total,
        human_bytes(j.speed as u64),
        eta
    )
}

fn summary(j: &Job) -> (String, ExitCode) {
    let elapsed = j.elapsed().map(fmt_duration).unwrap_or_else(|| "?".into());
    match j.status {
        Status::Done => {
            let files = match j.files_total {
                Some(total) => format!(", {} of {total} files transferred", j.files_transferred),
                None => String::new(),
            };
            let dry = if j.dry_run { " (dry run: nothing changed)" } else { "" };
            let mut s = format!(
                "job {} done{dry}: {} {}: {} in {}{}",
                j.id,
                j.op.as_str(),
                j.route(),
                human_bytes(j.bytes),
                elapsed,
                files
            );
            if let Some(w) = &j.error {
                s.push('\n');
                s.push_str(w);
            }
            (s, ExitCode::SUCCESS)
        }
        Status::Failed => (
            format!(
                "job {} failed after {}: {}\nFull log: afm log {}",
                j.id,
                elapsed,
                j.error.as_deref().unwrap_or("unknown error"),
                j.id
            ),
            ExitCode::FAILURE,
        ),
        Status::Cancelled => (
            format!("job {} was cancelled after {} ({}% done)", j.id, elapsed, j.percent),
            ExitCode::FAILURE,
        ),
        Status::Lost => (
            format!(
                "job {} is lost: its worker process stopped without a result. Full log: afm log {}",
                j.id, j.id
            ),
            ExitCode::FAILURE,
        ),
        _ => (progress_line(j), ExitCode::SUCCESS),
    }
}

fn wait(id: &str, timeout: Option<u64>, mut child: Option<Child>) -> Result<ExitCode> {
    let start = Instant::now();
    let mut last_report = Instant::now();
    loop {
        if let Some(c) = child.as_mut() {
            let _ = c.try_wait();
        }
        let j = Job::load(id)?;
        if j.status.is_final() {
            let (text, code) = summary(&j);
            println!("{text}");
            return Ok(code);
        }
        if timeout.is_some_and(|t| start.elapsed() >= Duration::from_secs(t)) {
            println!("{} (still active; run `afm wait {id}` again)", progress_line(&j));
            return Ok(ExitCode::from(EXIT_TIMEOUT));
        }
        if last_report.elapsed() >= Duration::from_secs(60) {
            println!("{}", progress_line(&j));
            let _ = std::io::stdout().flush();
            last_report = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

fn status(id: Option<String>, json: bool) -> Result<ExitCode> {
    if let Some(id) = id {
        let j = Job::load(&resolve_id(&id)?)?;
        if json {
            println!("{}", serde_json::to_string_pretty(&j)?);
            return Ok(ExitCode::SUCCESS);
        }
        println!("id:       {}", j.id);
        println!("op:       {}", j.op.as_str());
        println!("status:   {}", j.status.as_str());
        if let Some(n) = &j.name {
            println!("name:     {n}");
        }
        for s in &j.sources {
            println!("source:   {s}");
        }
        println!("target:   {}", j.target);
        println!(
            "progress: {}% ({}{})",
            j.percent,
            human_bytes(j.bytes),
            j.total_estimate().map(|t| format!(" of ~{}", human_bytes(t))).unwrap_or_default()
        );
        if let Some(t) = j.files_total {
            println!("files:    {}/{}", j.files_done, t);
        }
        if !j.status.is_final() {
            println!("speed:    {}/s", human_bytes(j.speed as u64));
            println!(
                "eta:      {}",
                j.eta.map(|e| format!("{} (ends {})", fmt_duration(e), eta_clock(e))).unwrap_or_else(|| "?".into())
            );
        }
        if let Some(e) = j.elapsed() {
            println!("elapsed:  {}", fmt_duration(e));
        }
        if let Some(e) = &j.error {
            println!("error:    {}", e.replace('\n', "\n          "));
        }
        return Ok(ExitCode::SUCCESS);
    }

    let jobs = list_jobs();
    if json {
        println!("{}", serde_json::to_string_pretty(&jobs)?);
        return Ok(ExitCode::SUCCESS);
    }
    if jobs.is_empty() {
        println!("no jobs");
        return Ok(ExitCode::SUCCESS);
    }
    println!("{:<6}  {:<4}  {:<9}  {:>4}  {:>9}  {:>7}  {}", "ID", "OP", "STATUS", "PCT", "BYTES", "ETA", "ROUTE");
    for j in jobs {
        let eta = if j.status.is_final() {
            String::new()
        } else {
            j.eta.map(fmt_duration).unwrap_or_else(|| "?".into())
        };
        println!(
            "{:<6}  {:<4}  {:<9}  {:>3}%  {:>9}  {:>7}  {}",
            j.id,
            j.op.as_str(),
            j.status.as_str(),
            j.percent,
            short_bytes(j.bytes),
            eta,
            j.route()
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// Cancel a job. Exit code 0 when the job has ended, 1 when it is still active.
fn cancel(id: &str) -> Result<ExitCode> {
    let mut j = Job::load(id)?;
    if j.status == Status::Lost {
        // The worker is gone; stop an rsync that still runs.
        if let Some(pid) = j.rsync_pid {
            if proc_comm(pid).as_deref() == Some("rsync") {
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGTERM);
                    libc::kill(-(pid as i32), libc::SIGCONT);
                }
            }
        }
        j.status = Status::Cancelled;
        j.finished = Some(now());
        j.save()?;
        println!("job {id} cancelled");
        return Ok(ExitCode::SUCCESS);
    }
    if j.status.is_final() {
        println!("job {id} is already {}", j.status.as_str());
        return Ok(ExitCode::SUCCESS);
    }
    send_control(id, "cancel")?;
    let start = Instant::now();
    loop {
        let j = Job::load(id)?;
        if j.status.is_final() {
            println!("{}", summary(&j).0);
            return Ok(ExitCode::SUCCESS);
        }
        if start.elapsed() >= Duration::from_secs(15) {
            println!("job {id} is still {} 15 s after the cancel request", j.status.as_str());
            return Ok(ExitCode::FAILURE);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn control(id: &str, cmd: &str, required: Status) -> Result<ExitCode> {
    let j = Job::load(id)?;
    if j.status != required {
        bail!("job {id} is {}, not {}", j.status.as_str(), required.as_str());
    }
    send_control(id, cmd)?;
    println!("sent {cmd} to job {id}");
    Ok(ExitCode::SUCCESS)
}

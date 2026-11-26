# afm — agent file manager

`afm` copies and moves files and directories with rsync. Each transfer is a job.
`afm dash` shows all jobs live: name, progress, size, speed, ETA and agent, plus source, target
and a speed graph for the selected job.

## Install

Requires Linux, rsync and a Rust toolchain.

```
cargo install --git https://github.com/pglira/afm
```

## Use

- Terminal 1: `afm dash`
- Agent: press `i` in the dashboard to copy the skill install prompt (`INSTALL_PROMPT.md`), and
  paste it into the agent prompt. The agent then installs the skill with
  `afm skill > ~/.claude/skills/afm/SKILL.md`. Do this one time; the skill then works in all sessions.
- `afm skill` prints the skill (`SKILL.md`). `afm install-prompt` prints the install prompt.

```
afm cp -n NAME [-d] SOURCE... TARGET [-- RSYNC_OPTIONS...]
afm mv -n NAME [-d] SOURCE... TARGET [-- RSYNC_OPTIONS...]
afm status [ID] [--json] | wait ID [--timeout SEC] | log ID | cancel ID | pause ID | resume ID | clear
```

## Dashboard keys

| Key | Action |
|---|---|
| ↑ ↓ / j k, mouse click or wheel | select a job |
| p / Space | pause or resume |
| c | cancel (asks for confirmation) |
| l / Enter | rsync log (scroll with ↑ ↓, PgUp, PgDn, g, G) |
| d | remove the selected finished job |
| i | copy the skill install prompt to the clipboard |
| x | remove all finished jobs |
| q | quit |

## How it works

- `afm cp`/`afm mv` starts a detached worker process (`afm __run ID`) in its own session.
  A job continues when the calling shell is killed.
- The worker runs `rsync -a --partial --info=progress2,name1 --no-inc-recursive`,
  parses the progress output and writes `job.json` atomically.
- Jobs are in `$AFM_STATE_DIR`, else `$XDG_STATE_HOME/afm`, else `~/.local/state/afm`.
  Each job directory holds `job.json`, `rsync.log` and a `control` file for pause, resume and cancel.
- Pause and cancel send `SIGSTOP`/`SIGCONT`/`SIGTERM` to the rsync process group, which includes `ssh`.
- `mv` uses `--remove-source-files`, then removes the empty source directories (also on a remote host over SSH).
- SSH runs with `BatchMode=yes` unless `RSYNC_RSH` is set, so a password prompt cannot block a job.
- `-x, --exact` adds `-HAX` (hard links, ACLs, extended attributes).
- At the end of a job, the worker sends a desktop notification (`notify-send`): always for a failed
  job, and for a done job that ran at least `$AFM_NOTIFY_MIN_SECS` seconds (default 10).
  `AFM_NOTIFY=0` turns notifications off. The worker uses the environment of the process that started the job.
- The agent name is `--agent`, else `$AFM_AGENT`, else the first parent process that is not a shell.

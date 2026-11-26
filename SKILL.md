---
name: afm
description: Copy or move files and directories with the afm command (rsync jobs that the user watches in a live dashboard). Use this skill for every copy or move of files or directories, local or over SSH, instead of cp, mv or rsync.
---

# File transfers: use `afm`

Use the `afm` command for every copy or move of files and directories. Do not use
`cp`, `mv` or `rsync` directly. `afm` runs rsync, and the user watches all `afm`
jobs live in a dashboard.

## Commands

```
afm cp -n NAME [-d] [-x] SOURCE... TARGET [-- RSYNC_OPTIONS...]   # copy
afm mv -n NAME [-d] [-x] SOURCE... TARGET [-- RSYNC_OPTIONS...]   # move
afm status [JOB_ID] [--json]       # one job, or all jobs
afm wait JOB_ID [--timeout SEC]    # wait until the job ends
afm log JOB_ID [-n LINES]          # rsync output and errors
afm cancel JOB_ID                  # stop a job
```

- `-n, --name "NAME"` (required): give every operation a short, unique name that
  tells what it is, for example `-n "backup raw data exp42"`. The dashboard shows
  this name, so make it clear for a human. Use 2 to 6 words.
- `-d, --detach`: return at once and print the job id. The job runs in the background.
- A job id prefix is enough, for example `afm wait a3f`.

## Path rules (rsync rules; `-n` is not shown in these examples)

- `afm cp /data/photos /backup/`  → creates `/backup/photos/...`
- `afm cp /data/photos/ /backup/photos-2024`  → copies the *contents* of `photos`
  into `/backup/photos-2024`
- A trailing slash on a SOURCE directory means "the contents of this directory".
  No trailing slash means "this directory itself". `dir/.` and `.` also mean "the
  contents" (the same as `dir/` and `./`).
- Remote paths use SSH: `host:/path` or `user@host:/path`. SSH must work without a
  password prompt (key-based login). One side must be local.
- `afm mv` renames a local source that is on the file system of its destination, when
  the destination does not exist and there are no extra rsync options. This is instant
  for any size. For all other sources, rsync moves the data.
- With rsync, `afm mv` deletes each source file only after its transfer is complete. At the end,
  it removes the empty source directories. It keeps them when the rsync options contain
  filters (`--exclude`, `--include`, `--filter`, `--files-from`, ...) or a dry run.
- `afm` refuses a transfer when the source and the destination are the same path, when
  the destination is inside the source directory, or when `--delete` would remove the
  source. Check the paths. Use `--force` only when the user tells you to.
- An existing file at the target is overwritten when it differs. Unchanged files are
  skipped, so a repeated command continues an interrupted transfer. For such a
  continued transfer, the percent and the ETA are rough.

## Blocking and long transfers

- Without `-d`, the command blocks until the job ends and prints a summary.
- For large or slow transfers (more than a few GB, or more than a few minutes), use
  `-d`, then call `afm wait JOB_ID --timeout 500` until it exits with a code other
  than 124. Keep each call shorter than the timeout of your shell tool.
- If your shell tool kills a blocking `afm cp`/`afm mv`, the job continues in the
  background. Find it with `afm status` and continue with `afm wait`.
- Do not start the same transfer again while a job for it is active.

## Exit codes

- `0`: the job is done. For `afm cancel`: the job has ended.
- `1`: one of these:
  - An error before a job exists: the output starts with `afm: error:` (for example a
    missing source or a refused path). There is no job and no log. Fix the command.
  - The job failed, was cancelled or is lost. Read the printed error and `afm log JOB_ID`.
  - `afm cancel`: the job did not stop within 15 seconds.
- `2`: wrong command-line usage.
- `124`: `afm wait --timeout` ended before the job. The job is still active.

## Rules

- If the user cancels a job (status `cancelled`), do not start it again. Ask the user.
- If a job fails, read `afm log JOB_ID` before you try again. Do not retry blindly.
- Give extra rsync options after `--`, for example
  `afm cp -n "sync src" src/ dst/ -- --exclude '*.tmp' --dry-run`. Do not give progress or output
  options (`-v`, `--progress`, `--info`), because `afm` sets them.
- Use `--dry-run` (after `--`) to preview a large or risky move. A dry run changes
  nothing; its summary says `dry run: nothing changed`.
- File attributes: by default, `afm` keeps permissions, times, owners (when allowed) and
  symlinks (`rsync -a`). Add `-x, --exact` to also keep hard links, ACLs and extended
  attributes. Use `--exact` for backups, for system or home directories, and when the
  user wants an exact copy. Do not use it when the target is a FAT, exFAT or NTFS drive
  or a cloud mount, because these do not support the attributes and the job fails.

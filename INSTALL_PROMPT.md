Install the `afm` skill for file transfers. `afm` is a command-line tool on this computer that copies and moves files with rsync. I watch all `afm` jobs in a live dashboard.

1. Make sure that `afm` is on the PATH: run `afm --version`. If this fails, stop and tell me.
2. Create the skill. For Claude Code, install it as a personal skill, so that it works in all projects:

   ```
   mkdir -p ~/.claude/skills/afm && afm skill > ~/.claude/skills/afm/SKILL.md
   ```

   If you are not Claude Code, save the output of `afm skill` where your agent keeps skills or instructions.
3. Read the new SKILL.md file. Follow it from now on: use `afm` for every copy or move of files and directories, and give each operation a name with `-n`.
4. Tell me in one sentence that the skill is installed. Then wait for my file transfer requests.

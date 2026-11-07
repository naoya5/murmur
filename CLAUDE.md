# CLAUDE.md

The rules for this repository live in `AGENTS.md`, shared with Codex and other agents. Claude Code
loads it through this import:

@AGENTS.md

## Claude Code notes

- Project skills: `.claude/skills` is a symbolic link to `.agents/skills`. Add a skill there once
  and both Claude Code and Codex load it.
- Keep this file to notes that only Claude Code needs. Everything else goes in `AGENTS.md`.

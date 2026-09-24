# Claude Code configuration

Claude Code reads root `CLAUDE.md`; project settings are in `.claude/settings.json`, path-scoped rules in `.claude/rules/`, specialist definitions in `.claude/agents/`, and skills in `.claude/skills/`. That last path is a relative symlink to canonical `skills/`, so there is one editable source.

The settings use supported command hooks to invoke shared scripts: a `PreToolUse` Bash guard blocks narrowly identified destructive commands, a write guard rejects credential filenames, and a lightweight `Stop` check runs repository sanity without the full suite. Agents use the stable `opus` alias rather than a dated model ID.

Project settings are reviewed code, not a security sandbox. Developers must trust the repository before enabling hooks. If a platform does not follow symlinks, configure its skill search path to `skills/`; do not duplicate bodies. Official documentation was unreachable in this bootstrap environment, so validate schema behavior with the installed Claude version before relying on hooks in a release workflow.

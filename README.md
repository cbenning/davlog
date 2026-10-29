# davgit (working name)

A small WebDAV server that serves a git working tree and turns client writes into git commits, with optional periodic pushes to a remote and pluggable alerting (email first).

**Status: implementation in progress.** The design was approved on 2026-10-28; the build follows `docs/superpowers/plans/2026-10-28-davgit-v1.md`. See `AGENTS.md` before doing anything.

## Documents

| File | Purpose |
|------|---------|
| `AGENTS.md` | Read first. Ground rules and next steps for any agent working here. |
| `docs/CONTEXT.md` | Why this exists, how the idea arose, what the owner prefers, prior art considered. |
| `docs/DESIGN.md` | The approved design: goals, decisions, architecture, config, behaviour. |
| `docs/CONCERNS.md` | Hard parts and edge cases worked through so far, with proposed handling. |
| `docs/OPEN-QUESTIONS.md` | Decisions that are not made yet. |

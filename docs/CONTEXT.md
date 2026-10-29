# Context

## Origin

While working on a headless WebDAV sync CLI for HelixNotes (a notes app; branch `feat/helix-cli` in `~/workspace/HelixNotes`), the owner asked whether a simple WebDAV server exists that is backed by git. (The owner's list of requirements was cut off after the first item, "backed by git". It is not known whether they had more. Ask if it matters.)

The inferred motivation: a self-hosted WebDAV endpoint for notes (and similar plain-file data) where every change is versioned, history is browsable and revertable with ordinary git tools, and the repo can be mirrored to a remote as an off-site backup.

## Prior art considered (from the agent's memory, not verified by search)

No off-the-shelf "WebDAV server with a git backend" was found or known. Nearest options:

- A plain WebDAV server (`rclone serve webdav`, `dufs`, `wsgidav`, nginx `dav_ext`, Apache `mod_dav`) on a git working tree, plus an auto-commit watcher such as `gitwatch`. Works, but commit behaviour is crude, there is no push policy, and no alerting.
- `wsgidav` with a custom provider that commits on each write. Python, would need writing.
- Radicale: CalDAV/CardDAV only, but has a built-in "commit every change to git" hook. Good model for the pattern.
- Apache `mod_dav_svn` with autoversioning: every write becomes an SVN revision. Subversion, not git.

Before building, a quick web search for newer projects is worthwhile (the knowledge above may be stale).

## Decision to build our own

Estimated at about one day for an MVP and 2 to 4 days for a solid version. Rust was chosen over Go (Go's `golang.org/x/net/webdav` would have been the quicker path).

## Rejected design: serve straight from git objects

Serving from a bare repo with no working tree (each PUT becomes blob, tree and commit plumbing) was considered and rejected. It is much more work (roughly two weeks) and gains nothing for the target use case. We serve a normal working tree and commit it.

## The sibling client (HelixNotes) in brief

HelixNotes is a Tauri desktop app (Svelte frontend, Rust in `src-tauri`). Its `helix-sync` crate is a WebDAV sync engine and `helix-cli` is a headless sync of a vault folder. Recent commit messages mention "never rewrite notes during sync", so it is careful about not mutating files. It is expected to use plain PROPFIND, GET, PUT, MKCOL, DELETE and MOVE and probably not LOCK. The server is expected to run in a Docker container. This server must still work with other clients (rclone, davfs2, cadaver, macOS Finder, Windows Explorer, Cyberduck, other notes apps).

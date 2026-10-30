# Changelog

All notable changes are listed here. Versions follow [Semantic Versioning](https://semver.org/); while the major version is 0, anything may change.

## Unreleased

Fixes from a review of the code:

- **A commit made outside davlog is no longer undone.** davlog kept its own copy of git's index and never reloaded it, so after a person used git in the repository (a rebase to fix a diverged remote, say), the next davlog commit could record their changes as deletions. It now reloads the index before every commit.
- **A short upload no longer replaces the file.** A chunked upload that announced a length (as macOS Finder does) but sent fewer bytes could replace the real file with the partial data before the error was reported. The upload is now refused and the old file kept.
- **One unreadable file no longer blocks every commit.** A path davlog cannot read is skipped with a warning and the rest of the batch is committed.
- **Shutdown makes its final commit first.** A push stuck on an unresponsive remote could delay or prevent it. Background tasks now get five seconds to stop afterwards.
- **Startup no longer waits on an unresponsive remote.** The check against the remote for an existing repository gives up after five seconds.
- **Repacking no longer fails a commit now and then.** It removed empty object directories that a commit could be writing into.
- **Zero intervals and an empty commit author are rejected** at startup instead of panicking or hanging.
- **The alert for a stuck `.git` state** now mentions a stale `index.lock` or rebase directory.

## 1.0.0-RC2 - 2026-10-29

First release candidate. It has been tested with rclone, the litmus suites and a strict notes-sync client, but not widely used, so please report anything that breaks.

- WebDAV server over a git working tree. Every upload, delete, move and copy becomes a commit, with `per_request`, `debounce` and `interval` commit modes.
- Uploads are written atomically through a temp file, so a commit never sees half a file. `.git` and names that imitate it are hidden, and symlinks are never followed.
- Optional push to a backup remote on a timer. davlog never rebases, merges or force-pushes. If the remote has changes it doesn't know about, it stops pushing, emails you the commands to fix it, and resumes by itself once a push would be a fast-forward.
- Email alerts through a pluggable `Notifier` trait, rate-limited, with "recovered" notices.
- It stands down (writes get `503`) while you have a rebase or merge in progress in its repository.
- Sets itself up: creates the repository on first start, and asks (or exits with code 10 in a container) when the right answer isn't obvious.
- Limits against abuse: file size, free disk and repository size, connections, slow headers and stalled uploads. Failed logins are logged, and the delay on a wrong password is configurable.
- Repacks its own repository in the background without ever deleting an object.
- A Docker image for linux/amd64 on `ghcr.io/cbenning/davlog` (arm64 is not published yet), with Compose files for trying it on localhost and for running it behind Caddy with automatic HTTPS.

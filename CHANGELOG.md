# Changelog

All notable changes are listed here. Versions follow [Semantic Versioning](https://semver.org/); while the major version is 0, anything may change.

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

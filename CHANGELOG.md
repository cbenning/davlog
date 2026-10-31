# Changelog

All notable changes are listed here. Versions follow [Semantic Versioning](https://semver.org/); while the major version is 0, anything may change.

## 1.0.0-RC5 - 2026-10-31

Two speed fixes, found by measuring request times for the README:

- **Reads and other responses no longer wait 40 ms.** Connections now use `TCP_NODELAY`. Without it, a response sent in two parts (headers, then the body) waited for the client's delayed acknowledgement, so every file download took about 40 ms instead of well under a millisecond.
- **Commits are about ten times faster in a large repository.** RC3 reloaded the whole index from disk before every commit, which cost about 40 ms with 5,000 files. The index is now reloaded only when the file on disk changed, so a commit made outside davlog is still picked up. A save in a 5,000-file repository commits in about 4 ms.
- The README has a new Performance section with the measurements.

## 1.0.0-RC4 - 2026-10-30

A new endpoint for monitoring, and a second round of fixes, found by reviewing failure modes and by a new soak test that has several clients upload, delete, move and copy at once:

- **`GET /readyz`** answers 200 when nothing needs attention and 503, with a list of what is wrong, when pushing is paused, a person has a rebase or merge going, the disk is too full, commits or pushes are failing, or the committer has stopped. `/healthz` still only says the process is alive, and now also shows `commit_failing` and `push_failing`. Neither needs a login, and neither shows file names.
- **Commits no longer fail when files change underneath them.** With several clients writing, staging a folder could race with another request deleting or replacing a file in it, and the request got a 500 even though its change had saved. Staging now retries a few times, and anything still changing is left for the next commit.
- **Logins are cheaper and can no longer stall the server.** An argon2 hash was checked on a core request thread for every request. It is now checked off those threads, at most four at a time, and a good login is remembered for five minutes. A wrong password is never remembered, so every guess still pays the full cost.
- **An empty password or username is rejected at startup** (`DAVLOG_PASSWORD=""` used to let anyone in).
- **A flapping problem no longer floods your inbox.** A condition that kept crossing a threshold (free disk, say) sent an alert and a recovery every few seconds. A recovery is now sent only for an alert that went out, and a problem that returns within an hour of recovering is held back.
- **A client that stops reading a download is cut off** after `limits.body_idle_timeout` instead of holding its connection slot forever.
- **davlog exits if a core part dies.** If the committer, pusher or another background task stopped unexpectedly, the server kept accepting uploads that were never committed. It now exits so the container restarts.
- **Cherry-picks and reverts in progress** now count as a person working in the repository, like rebases and merges.
- A failing `accept()` (out of file descriptors) no longer spins.

## 1.0.0-RC3 - 2026-10-30

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

# Changelog

All notable changes are listed here. Versions follow [Semantic Versioning](https://semver.org/); while the major version is 0, anything may change.

## Unreleased

- **A repack that runs out of memory is killed in preference to the server.** The repack process now asks the kernel to pick it first when a container hits its memory limit. In a 13-hour soak test with a 2 GB repository in a 256 MiB container, the repack was killed 8 times and the server kept serving, but the server itself had been killed 7 times earlier, when the kernel picked it instead.
- **A deletion could be lost, leaving the file in git forever.** When a folder is deleted, `dav-server` removes its children one by one and drops the ones still in flight if one fails. A dropped request never reached the code that reports the change, although the file had already been removed from disk. The deletion was then never committed, so the file stayed in git (and would come back if the working tree were ever restored from it). Every file system change and its report now run as one task that a dropped request cannot cancel. Found by the soak test failing about 1 run in 60 on CI.
- **The commit log line says what changed.** `committed update notes/a.md oid=b7bdfcc` replaces `committed oid=<40 hex digits> changes=1`. When several writes are committed together it lists the first five.
- **The push and repack log lines say what they did.** `pushed 4 commits branch=main`, `packed 9 loose objects into a new pack`, and `repacked everything into one pack (26 objects)`.

## 1.0.0-RC8 - 2026-11-03

Found by a soak test that wrote about 80,000 commits into a container with 256 MiB of memory:

- **A big repack can no longer kill the server.** Repacking needs memory in proportion to what it packs. In a repository that was large for its memory limit, it ran the whole container out of memory, which restarted, repacked, and ran out again. The repack now runs in a separate process (`davlog maintain-worker`), so running out of memory fails only the repack: the server keeps serving, and you get a "maintenance failed" email.
- **Most repacks are much smaller.** They pack only the loose objects. Everything is rewritten into one pack only once there are more than `pack_limit` packs.
- **A crash no longer leaves the server stuck.** If davlog was killed in the middle of a commit, it left `.git/index.lock` behind, and every write then returned 503 until someone deleted the file. A lock older than 5 minutes is now removed (with a warning in the log). A newer one is still treated as a person's git command.
- Repacking and pushing use less memory: one packing thread, freed memory handed back to the system, and the commit handle refreshed after a repack so it stops holding the replaced packs open.

## 1.0.0-RC7 - 2026-11-02

- **Memory no longer grows with every commit.** libgit2 kept every tree and commit it read in an in-memory cache, up to 256 MiB. Each commit reads back the trees it has just written, and nothing reads them again, so memory grew by several KB per commit until the process reached the cache limit. A container with a low memory limit was killed for it. These objects are no longer cached. Memory now stays flat: in a 13-minute test with six clients writing at once, it grew from 4 to 6 MiB over about 4,000 commits, where it used to grow by about 8 KB per commit. Commit and read times are unchanged.
- A new test, `tests/memory.rs`, fails if memory grows with the number of commits.

## 1.0.0-RC6 - 2026-11-02

- **Overwriting a file keeps its permissions.** A save is written to a temp file and renamed over the original, and the temp file had default permissions, so saving an executable script over WebDAV dropped its execute bit and git recorded a mode change. The replaced file's permissions are now kept.
- The README is shorter, with fewer long paragraphs.

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

# Design (approved 2026-10-28)

Legend: **Decided** = the owner confirmed it. **Guess** = the previous agent's proposal, open to change.

## Intent

Run one small server that exposes a directory over WebDAV. The directory is a git working tree. Every change a client makes ends up in a git commit, with commit timing that the operator can configure. On a timer the server pushes to a remote, and when something goes wrong (for example a divergent remote) it alerts the operator.

## Goals

- Standard WebDAV that works with many clients. Decided: no assumption about which client is used.
- Reliable, automatic versioning of everything written through the server.
- Configurable commit behaviour (see below).
- Periodic push to a remote, with sane handling when the remote has moved ahead.
- Alerting through an abstraction with multiple possible channels. Email first.
- Small, single static-ish binary, simple config file, easy to run as a systemd service or container.

## Non-goals

- Multiple users, multiple repos, per-user permissions (Decided: one repo, one user, per instance. Run several instances for several repos).
- Serving from git objects without a working tree.
- Being a git hosting server (no smart-HTTP git protocol).
- Merging user content or resolving conflicts automatically. The server never rebases or merges (Decided 2026-10-28).
- Anything else writing to the remote branch. Unsupported; the server detects divergence and pauses pushing.
- Horizontal scaling and multiple writers per repo (Decided: single writer per repo; scale by running one instance per repo). Hot failover is a possible later feature, not now.
- A web UI.

## Anti-goals (would be a failure even if it technically works)

- Losing or silently overwriting data, including on a rejected push. Never force-push by default.
- A client sync leaving the repo in a corrupt or half-finished git state.
- Clients being able to read or modify `.git`.
- Alert spam, or alerting failures that block serving.
- Shelling out to the `git` binary.

## Decided

| Topic | Decision |
|-------|----------|
| Language | Rust |
| Git access | A complete git library, no shelling out |
| Scope | One repo, one user (basic auth) per instance |
| Directory ownership | The server owns the directory exclusively. No one else pulls or edits it |
| Commit modes | Configurable: debounced batch, fixed interval, per request. Default: `per_request`, unless the benchmark shows it is too slow, in which case fall back to `debounce` (owner, 2026-10-28) |
| Remote | Periodic push on a timer |
| Push rejected | Remote divergence is unsupported. Fetch for diagnosis only, pause pushing, alert, keep serving and committing. No rebase, no merge. Resumes automatically when a push is a fast-forward again (supersedes the earlier "fetch, rebase, retry" decision) |
| Alerting | Abstracted so it can notify several ways. Email is first |
| Clients | Do not assume any specific WebDAV client |
| Project | Standalone, unaffiliated with HelixNotes. Working name `davgit` |
| Alert events | Any problem: divergent remote, push failures, and other errors (commit failure, bad repo state). The exact list is the builder's to flesh out, with "anything that goes wrong" as the rule |
| Failure behaviour | On any push problem, keep serving and keep committing locally. Only pushing stops |
| Per-request commit timing | Configurable whether the HTTP response waits for the commit. Default: wait (`wait_for_commit = true`) |
| Repo bootstrap | The server owns the repo. No manual git setup. It creates the repo if the directory has none, and initial content and the first commit come through WebDAV |
| Empty directories | Owner delegated: the server may do whatever it needs. Builder's call (see Left to the builder) |
| Max file size | Supported and configurable |
| Periodic push | Optional. With no `[push]` section (or `enabled = false`) the server is local-only: no pusher task, no remote, no push alerts |
| Remote role | Write-only backup. Concurrent writes to the remote branch are explicitly unsupported |
| Scaling | Out of scope. Single writer per repo. Request-handling parallelism is configurable (`worker_threads`); git operations stay serialized. Hot failover is a possible later option |
| Git library | `git2`. Re-checked 2026-10-28: `gix` has no rebase and only partial push. See Guesses for details |

## Guesses (proposed by the previous agent, unconfirmed)

- **Git library:** `git2` (libgit2 bindings). The design needs commit, fetch and push, not rebase. `gix` (gitoxide) was re-checked on 2026-10-28 against its `crate-status.md`: fetch, commit creation, status and low-level checkout are done, push is partial, rebase and reset are listed as still needing plumbing. A third-party mirror dated 2026-05-07 agrees. So `git2` is the pick; re-check `gix` in a few months. Note `git2` links libgit2 (and OpenSSL/libssh2 unless features are chosen carefully), which affects static builds. Also note libgit2 has no `gc` or repack.
- **WebDAV layer:** the `dav-server` crate (hyper/axum friendly) with `LocalFs` or a thin wrapper around it. Verify it can hook write completion and hide paths. If it cannot, write a wrapper `DavFileSystem`.
- **Change detection:** the wrapper sets a "dirty" flag on any mutating operation. The committer stages with a full add-all equivalent (`index.add_all` plus `update_all` for removals), so it does not need to track individual paths. Commit is skipped when the tree matches HEAD.
- **Email:** `lettre` crate, SMTP with STARTTLS or implicit TLS, credentials from config or an environment variable.
- **Config format:** TOML.
- **Async runtime:** tokio. libgit2 calls are blocking, so run them under `spawn_blocking` behind one async mutex.

## Architecture (proposed)

Modules, roughly:

- `config`: load and validate TOML, env overrides for secrets.
- `dav`: HTTP server, basic auth, the WebDAV filesystem wrapper. The wrapper hides `.git`, writes atomically, tracks dirty state and signals the committer.
- `repo`: thin wrapper around `git2::Repository`. All git operations go through one mutex-guarded handle. Exposes `commit_all(message)`, `fetch`, `push`, ahead/behind and ancestry helpers, status helpers.
- `committer`: the commit scheduler. Three strategies behind one trait or enum: debounce, interval, per-request. Only one strategy is active per instance.
- `pusher`: timer-driven push loop with divergence detection (fetch for diagnosis, no rebase) and an in-memory "paused" state.
- `notify`: `Notifier` trait, event type, rate limiter, and the email implementation.
- `main`: wiring, signal handling, graceful shutdown (flush a final commit).

## Repository bootstrap

Decided: the server owns the repository and the operator does not need to prepare it with git by hand. If `repo_path` is not a git repository, the server creates one (on the configured branch, with the configured remote if any). Initial content arrives through WebDAV, and the first commit is made by the server after the first write.

Decided: when the situation is ambiguous at first start, the service asks the operator what to do. Ambiguous means any of: `repo_path` is not a repo and a remote is configured whose branch already has history; `repo_path` holds files but is not a repo; local and remote histories are unrelated. The unambiguous cases need no question: an existing repo is used as is, and an empty directory with an empty remote is initialised.

Decided: the likely deployment is a Docker container, so printing to the console is fine. How the question works:

- With a TTY (for example `docker run -it` or `docker compose run`), the service prints the situation and the choices (clone the remote, initialise fresh and keep local files, or abort) and reads the answer from stdin.
- Without a TTY, the service must not guess. It logs a clear message to stderr naming the setting that answers the question, then exits with a distinct non-zero exit code. The setting is available both as a flag and as an environment variable (for example `--bootstrap clone|init|abort` and `DAVGIT_BOOTSTRAP`), since containers are usually configured through the environment.
- Guess: a container with a restart policy will loop on that exit. Document it, and consider a short sleep before exiting so logs stay readable.
- After bootstrap the repo exists, so the question never comes up again.

## Deployment

Decided (likely): runs as a Docker container. The repo directory is a mounted volume. Secrets (password, SSH key, SMTP password) come from mounted files or environment variables. TLS is terminated by a reverse proxy in front.

Guess: ship a Dockerfile and an example `compose.yaml`. Check how `git2` links (libgit2, OpenSSL, libssh2) before choosing a base image; a vendored build or a distroless/alpine image may need extra care. A `/healthz`-style endpoint suits container health checks. Log to stdout and stderr only.

## Commit behaviour

Configurable via the config file. One mode is active per instance.

- **debounce** (fallback default if per_request proves slow): after the first write, wait `debounce` seconds of quiet (optionally capped by `max_wait` so a continuous stream still commits), then commit everything dirty. Good fit for sync clients that upload many files in a burst.
- **interval**: every `interval`, commit if the tree is dirty, regardless of activity.
- **per_request**: commit when each mutating request completes (PUT after the file is fully written and closed, DELETE, MKCOL, MOVE, COPY). Precise history, noisy. Should stage and commit only the affected paths where practical. Whether the HTTP response waits for the commit is configurable (`wait_for_commit`, default `true`). Decided. With `true`, a failed commit can be reported to the client as a 5xx.

Guess, group commit for `per_request`: while one commit runs, further completed writes queue and are committed together in the next commit. This acts as an adaptive debounce with no timer, and keeps a burst of 1000 uploads from becoming 1000 serialized index rewrites.

Commit cost, measured 2026-10-28 with `cargo run --release --example commit_bench` (5,000-file tree, local NVMe-class disk, libgit2 via `git2` 0.21, 200 samples per row):

| Case | Result |
|------|--------|
| Single-path commit (`commit_paths`) | median 4.7 ms, p95 5.6 ms |
| Add-all commit with one file touched | median 32.8 ms, p95 33.9 ms (grows with tree size) |
| Burst of 1,000 new files as 1,000 single-path commits | median 8.6 ms each, 8.5 s wall |
| Same burst as one 1,000-path (group) commit | 0.145 s wall |
| Single-path commit with `core.fsync=all` and `core.fsyncobjectfiles=true` in repo config | median 4.3 ms; no measurable change, so it is unverified whether libgit2 honoured the setting (`git2` does not expose `GIT_OPT_ENABLE_FSYNC_GITDIR`) |

Path-scoped staging is about 7 times cheaper than add-all on this tree, and group commit makes a burst about 60 times cheaper than serial commits. libgit2 does not fsync by default, which is fast but not crash-durable. Many small commits also leave many loose objects, and libgit2 cannot `gc`; see `CONCERNS.md` #21.

Default mode: Decided, `per_request` (with group commit). The benchmark above is well inside the agreed bar (median under 50 ms, 1,000-commit burst under 60 s), so the default stands.

Common to all modes: configurable author name and email, and a commit message template. Suggested default message: a one-line summary such as `davgit: update 3 files` followed by a body listing changed paths with their change type, truncated after a limit. In per_request mode the message can name the single path and operation.

A final commit runs on graceful shutdown and a startup check commits (or alerts about) a dirty tree left from a crash.

## Push behaviour

- Config: remote name or URL, branch, `interval`, credentials (SSH key path or HTTPS token), enable flag.
- On each tick: ensure no uncommitted changes (trigger a commit first), then push. If the push is rejected as non-fast-forward, the remote has diverged. See "Divergent remote" below. The server never rebases or merges.
- Never force-push by default. A `force` option could exist but is not planned.
- Pausing affects only pushing. Serving and local committing continue. Decided.
- Decided, resuming: on each push tick while paused, the server re-fetches and re-checks. If a push would now be a fast-forward (for example the operator fixed the remote or the repo), it pushes, resumes, and sends a "recovered" notice. Alerts while paused follow the rate limit.

### Pushing is optional

Decided. If the config has no `[push]` section, or `enabled = false`, the server never starts the pusher. It keeps committing locally. Bootstrap offers "clone the remote" only when a remote is configured. Commit failures and bad repo state still alert.

### Divergent remote (unsupported, handled safely)

Decided 2026-10-28, replacing the earlier "fetch, rebase, retry" rule. The remote is a write-only backup. Anything else writing to the remote branch (another clone, a forge web edit, a merged PR, a restore, a force-push) is **not supported**, and the documentation says so plainly. With frequent small commits and a single writer, a divergence should not happen in normal use. When it does, the server protects the data and tells the operator. It does not try to resolve anything.

When a push is rejected as non-fast-forward (or the remote is strictly ahead of local):

1. Fetch, for diagnosis only. Do not rebase, merge or fast-forward. Do not touch the local branch or the working tree.
2. Enter the **paused** state. Keep serving and committing locally.
3. Send one alert, rate limited, with a diagnosis (ahead and behind counts, the remote tip), a statement that concurrent remote writes are unsupported, and suggested commands (see "Alert hints").
4. On each push tick while paused, re-check as described under "Decided, resuming" above.

Consequences: no libgit2 rebase is used, so writes never block on a push, and there is no write-lock window for it. The cost is that backups stop until a human acts, which is the right behaviour for an unexpected divergence.

Humans working in the repo. Decided 2026-10-28. Because the server never rebases, any rebase or merge state in `.git` (`rebase-merge`, `rebase-apply`, `MERGE_HEAD`) or an `index.lock` the server did not create belongs to a person. While such state exists, the server skips commit and push ticks, logs why, and alerts. Mutating WebDAV requests (PUT, DELETE, MKCOL, MOVE, COPY, PROPPATCH, LOCK) return 503 with `Retry-After`, so nothing uncommitted piles up. Reads (GET, HEAD, PROPFIND, OPTIONS) continue. Requests already in flight finish. Normal service resumes when the state clears.

### Alert hints

Guess. Alerts carry a diagnosis and suggested commands built from `repo_path`, the remote and the branch, for example:

```
# remote has changes you want to keep: merge or rebase by hand, then push
git -C /srv/notes fetch origin
git -C /srv/notes rebase origin/main
# remote is wrong (restore, stray push): after checking, overwrite it
git -C /srv/notes push --force-with-lease origin main
```

Content: ahead and behind counts and the remote's last commit. Other failure kinds get their own hint: auth failure (check the SSH key or token), host key (check `known_hosts`), network (check connectivity).

### Possible later

Not planned for v1, kept here so the ideas are not lost:

- **Conflict branch** (`on_conflict = "branch"`): on divergence, create `davgit/conflict-<timestamp>` at HEAD, point HEAD at it without a checkout, and keep committing and pushing there so off-site backup continues. The operator later merges it into the home branch with a real merge commit (not a squash or forge rebase-merge, which break the ancestor check), and the server returns to the home branch when the conflict branch is an ancestor of it. HEAD would be the only state; a refs scan for unmerged conflict branches would supply awareness. Needs an owner decision on defaults and on online switch-back before building.
- **Automatic rebase** for setups where others legitimately push. Conflicts, an extra write-lock window and abort-on-error handling would come with it.
- **Hot failover**: an active instance plus a standby that fast-forwards from the remote, with fencing of the old active node. If both accept writes the pushes diverge, and the divergent-remote rule pauses pushing and alerts, so the failure is safe and loud. LOCK state is in memory and is lost on failover.

## Alerting

- `Notifier` trait taking an `Event` (kind, severity, short message, details, timestamp). The server holds a list of notifiers; adding a channel means one new implementation and one new config section. First implementation: SMTP email. Candidates for later: webhook, command hook, ntfy, matrix.
- Decided, events: any problem. That includes a divergent remote and push failures at minimum. Guess, the concrete list: push paused on divergent remote, repeated push failures (auth, network), commit failure, repo in a bad state at startup. A file rejected for size is not an alert: the client gets a 413 and the server logs it. Send a "recovered" notice when pushes resume.
- Guess, rate limiting: per event kind. Send the first alert immediately, then at most one reminder per configurable period (for example 24 h) while the condition persists.
- A notifier error is logged and never blocks serving, committing or pushing. Notification sending must not run while holding the repo mutex.

## Config sketch (illustrative, not final)

```toml
[server]
listen = "127.0.0.1:8080"
repo_path = "/srv/notes"
worker_threads = 0           # tokio worker threads for request handling; 0 = number of CPUs. Git operations stay serialized behind one mutex

[limits]
max_file_size = "100MiB"     # 413 Payload Too Large above this; "0" or omitted = unlimited. Default is a guess

[auth]
username = "notes"
password_file = "/etc/davgit/password"   # or password_hash

[commit]
mode = "per_request"          # per_request | debounce | interval
debounce = "5s"             # debounce mode only
max_wait = "60s"             # debounce mode only
interval = "5m"              # interval mode only
author_name = "davgit"
author_email = "davgit@localhost"
message_template = "davgit: {summary}"
wait_for_commit = true       # per_request mode: respond only after the commit finished

# All git behaviour is set here, never by the host: the system and global git
# config is ignored, and these keys are written to the repo-local config at startup.
# remote.* keys (use [push]), core.bare and core.worktree are rejected.
[git.config]
"core.autocrlf" = "false"

# The whole [push] section is optional. Omit it for a local-only server.
[push]
enabled = true
remote = "origin"
url = "git@example.com:me/notes.git"   # the server adds the remote itself
branch = "main"              # home branch
interval = "10m"
ssh_key = "/etc/davgit/id_ed25519"       # or token_file for https
known_hosts = "/etc/davgit/known_hosts"  # required with ssh_key

[notify]
rate_limit = "24h"

[[notify.email]]
smtp_host = "smtp.example.com"
smtp_port = 587
tls = "starttls"
username = "alerts@example.com"
password_file = "/etc/davgit/smtp-password"
from = "davgit <alerts@example.com>"
to = ["me@example.com"]
```

## Testing approach (Guess)

- Unit tests for the committer strategies with a fake clock.
- Integration tests using temporary directories and a local bare repo as the remote. Cover clean push, divergent remote, and recovery (see below).
- WebDAV conformance with the `litmus` test suite, plus smoke runs with `rclone`, `cadaver` and the HelixNotes CLI (`helix-cli`) as real clients.
- A mock `Notifier` to assert alert behaviour and rate limiting.
- Optional push: with no `[push]` section the server runs, commits, and starts no pusher and sends no push alerts.
- Divergent remote tests (all use a local bare repo as the remote):
  - A push rejected as non-fast-forward fetches for diagnosis only, leaves the local branch, HEAD and working tree unchanged, pauses pushing, and sends one alert with ahead and behind counts and suggested commands.
  - A remote strictly ahead of local is treated the same way: no fast-forward of the working tree.
  - While paused, WebDAV reads and writes keep working and local commits continue.
  - Alerts while paused follow the rate limit.
  - Recovery: after the remote is reset to an ancestor of local, or the operator merges by hand so local contains the remote tip, the next tick pushes, resumes, and sends a "recovered" notice.
  - No `--force` push is ever sent unless a force option is explicitly configured (none planned).
- Human working in the repo: with `.git/rebase-merge`, `.git/rebase-apply`, `MERGE_HEAD` or a foreign `index.lock` present, the server skips commit and push ticks, logs why, does not abort or modify that state, returns 503 with `Retry-After` on mutating requests while reads still work, and resumes when the state clears.
- Alert hints: the divergence alert lists ahead and behind counts, the remote tip, and commands built from the configured `repo_path`, remote and branch. Auth, host key and network failures each get their own hint.
- Parallelism: with `worker_threads` above 1, concurrent reads proceed while a commit runs, and git operations remain serialized (no `index.lock` errors under a concurrent write burst).

## Left to the builder

A throwaway benchmark of commit cost (path-specific staging versus add-all, with and without fsync, burst of 1000 writes), run before the default commit mode is chosen. Crate selection details, module layout, the exact event taxonomy, log format, packaging (Dockerfile, systemd unit), and the remaining items in `OPEN-QUESTIONS.md`.

Empty directories (owner delegated the choice): suggested default is to let them exist on disk only and not add placeholder files, with an optional `track_empty_dirs` setting that writes a `.gitkeep`. Pick whichever keeps clients consistent: a client that creates a folder and then lists it must still see it.

Max file size: the exact default and whether the limit is checked from `Content-Length`, while streaming, or both. A request without `Content-Length` (chunked) must still be cut off mid-stream and its temp file removed.

## As built (2026-10-28)

Where the implementation differs from, or settled, the text above. The plan is `docs/superpowers/plans/2026-10-28-davgit-v1.md`.

- **Staging is path-scoped in every commit mode.** A commit stages exactly the paths that changed (literal, not glob). Add-all is used only for the startup and shutdown cases. This replaces the "dirty flag plus add-all" guess.
- **Waiting for the commit happens in the HTTP layer.** After the WebDAV handler returns a 2xx for a mutating request, the server calls `Committer::flush()` (when `per_request` and `wait_for_commit`), so a request waits once. `dav-server` calls the filesystem once per child on a recursive DELETE or COPY, and `per_request` commits as soon as a change is recorded, so a recursive DELETE can make several commits.
- **Config additions:** `[push].url` (the server adds the remote itself) and `[git.config]` (every git setting comes from the app config; the host's system, global, XDG and program-data git config is ignored).
- **`/healthz` is unauthenticated** and returns only `last_commit_unix`, `last_push_unix`, `push_paused` and `foreign_git_state`.
- **Invalid UTF-8 in a path** gets 400 from the server before `dav-server` sees it.
- **Uploads** use a temp file in `.git/<TMP_DIR_NAME>` renamed on `flush`. `LOCK` on a missing path creates the lock but not an empty file.
- **`dav-server` quirk fixed:** PROPFIND `getetag` is emitted without quotes (the `ETag` header has them). The server quotes the value so strict clients see a strong ETag.
- **Dead properties** are not supported: `PROPPATCH` returns 501 (litmus `props` 11/14 and `locks` 38/41; every failure is a `PROPPATCH`).
- **SSH known_hosts:** plain entries only; hashed (`|1|`), `@revoked` and `@cert-authority` entries never match. SSH push has unit tests but was not run against a real SSH server.
- **fsync:** `git2` does not expose `GIT_OPT_ENABLE_FSYNC_GITDIR`. Setting `core.fsync` in `[git.config]` made no measurable difference in the benchmark, so whether libgit2 honours it is unverified.
- **Startup:** a dirty tree is committed as `davgit: commit changes found at startup` unless a person has git state in `.git`. With `[push]` configured, a HEAD on a different branch than `push.branch` is a refusal to start (with an alert).
- **Alerts:** each notifier has its own queue and worker; rate limiting happens when an event is raised; `Dispatcher::drain` delivers queued alerts before exit. A foreign-git-state monitor polls every 2 s.
- **Exit codes:** 0 clean (also after `abort`), 1 error, 2 bad usage or config, 10 needs a bootstrap answer (after a 5 s pause).
- **Real clients:** rclone round trip identical with every change committed; litmus `basic` 16/16, `copymove` 13/13, `http` 4/4; the HelixNotes CLI syncs two vaults through the server including edits and deletes.

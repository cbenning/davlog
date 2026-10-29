# davgit (working name)

A small WebDAV server that serves a git working tree and turns every client write into a git commit. It can push to a backup remote on a timer and alerts you by email (more channels can be added) when something goes wrong.

It was built for notes and similar plain-file data: every change is versioned, history is browsable and revertable with ordinary git tools, and the repository can be mirrored off-site. It makes no assumption about which WebDAV client you use. It has been run against rclone and the litmus conformance suite (results below).

One server instance serves one repository for one user. Run several instances for several repositories.

## Quick start

```sh
cargo build --release
cp config.example.toml davgit.toml        # edit repo_path, listen, auth, ...
printf '%s' 'choose-a-password' > password  # referenced by auth.password_file
target/release/davgit --config davgit.toml
```

Then point any WebDAV client at `http://127.0.0.1:8080/` with the username and password from the config. Check on it with `curl http://127.0.0.1:8080/healthz`.

`davgit --help` lists the options. The full set of settings, with comments, is in `config.example.toml`. Secrets can also come from `DAVGIT_PASSWORD`, `DAVGIT_SMTP_PASSWORD`, `DAVGIT_PUSH_TOKEN` and `DAVGIT_PUSH_SSH_PASSPHRASE`, which win over files.

### Docker

```sh
mkdir -p data secrets && sudo chown 10001 data
printf '%s' 'choose-a-password' > secrets/password
cp config.docker.example.toml davgit.toml    # edit
docker compose up -d
```

See `compose.yaml` for the details. The image does not terminate TLS: put a reverse proxy (Caddy, nginx, Traefik) in front of it. Basic auth over plain HTTP is not safe on an untrusted network.

## How it works

- **Commits.** In the default `per_request` mode each write is committed as soon as it completes, and the HTTP response waits for the commit (`wait_for_commit = true`). Writes that arrive while a commit runs are committed together (group commit). `debounce` and `interval` modes batch changes instead. A commit stages only the paths that changed. Commit messages look like `davgit: update notes/a.md` or `davgit: 3 changes` followed by a list.
- **Uploads are atomic.** A PUT is written to a temp file inside `.git` and renamed into place when it completes, so a reader or a commit never sees half a file. A dropped connection leaves nothing behind. Files above `max_file_size` (default 100 MiB) get a 413.
- **`.git` is invisible.** Clients get 404 for it on every method and cannot create, move or copy anything into it. Symlinks are never followed.
- **All git settings come from the app config.** The host's system and global git config is ignored. Put what you need under `[git.config]` (for example `"core.autocrlf" = "false"`); it is written to the repo-local config at every start.
- **Locking.** An in-memory lock system provides class 2 locking, which macOS Finder and Windows Explorer need.

### First start (bootstrap)

The server owns the repository: you do not run `git init` yourself. If `repo_path` has no repository it creates one. A question is needed when it cannot tell what you want:

- the directory has files but is not a repository,
- a remote is configured and its branch already has history, or
- the local and remote histories are unrelated.

On a terminal the server asks. Without one (a container) it must not guess: it prints the choices to stderr, waits 5 seconds so logs stay readable, and exits with **code 10**. Answer with `--bootstrap clone|init|abort` or `DAVGIT_BOOTSTRAP=clone|init|abort`: `clone` clones the remote, `init` creates a repository and keeps the local files, `abort` exits without changing anything. A container with a restart policy will loop on code 10 until you answer. Exit codes: 0 clean, 1 error, 2 bad usage or config, 10 needs a bootstrap answer.

### Pushing to a remote (optional)

Add a `[push]` section to push the configured branch every `interval`. Leave it out for a local-only server.

**The remote is a write-only backup. Anything else writing to the remote branch is not supported** (another clone, a web edit, a merged pull request, a restore, a force-push). If the remote has commits the server does not have, the server:

1. fetches, only to look; it never rebases, merges or fast-forwards your working tree and never force-pushes,
2. pauses pushing and sends an alert with the ahead and behind counts and the commands to fix it,
3. keeps serving and keeps committing locally,
4. tries again on every push tick and resumes by itself, with a "recovered" notice, once a push is a fast-forward again.

To fix it with the git CLI, work in the server's repository: merge or rebase the remote's changes by hand, or, if the remote is the one that is wrong, `git push --force-with-lease`. While you have a rebase or merge in progress there, the server stands down: it skips commits and pushes, and answers writes with `503` and `Retry-After` (reads keep working). It resumes when the rebase or merge is finished or aborted.

SSH remotes need `ssh_key` and a `known_hosts` file; unknown or changed host keys are refused. Only plain `known_hosts` entries are understood (hashed `|1|` entries are ignored). HTTPS remotes use `token_file`.

### Alerts

An event is sent for a divergent remote, repeated push failures (after 3 in a row), commit failures, a repository in a bad state, and a person's rebase or merge state. Each kind is sent at once, then at most once per `notify.rate_limit` (default 24 h) while it lasts, followed by a "recovered" notice. Email is the first channel; adding another means one new `Notifier` implementation and one config section. A failing notifier never blocks serving, committing or pushing.

### Housekeeping

- **Run `git gc` now and then** (a cron job is enough). libgit2 cannot repack, and `per_request` mode makes many small commits and loose objects.
- Git LFS is not supported.
- `GET /healthz` needs no login and returns only counters and state (last commit and push time, whether pushing is paused, whether a person has git state in the repo). It never lists file names.

## Known limits

- **Dead properties are not supported.** `PROPPATCH` returns 501, so modification times set by a client are ignored (git does not store them anyway).
- **Case and Unicode.** The server's filesystem is case-sensitive; a client on a case-insensitive system can create names that differ only in case. Unicode normalisation is not applied.
- **`LOCK` on a missing path** creates the lock but no empty file; the file appears when the client uploads it.
- **Empty directories** exist on disk but git does not track them, so they vanish from history.
- **Recursive `DELETE`** of a folder can produce several commits instead of one.
- **`/healthz` is reserved.** A file named `healthz` at the repository root cannot be reached over WebDAV.
- **A client-written `.gitignore` can stop versioning.** Files it ignores are served and stored but not committed; the server only logs a warning.
- **Single writer.** One instance per repository; there is no clustering or failover.
- **SSH push is covered by unit tests only.** It has not been exercised against a real SSH server in this repository's test run; try one push before relying on it.

### Test results

- Unit and integration tests: `cargo test`.
- rclone: `scripts/smoke.sh` syncs a tree up and down, compares it, and checks the commits. It also runs litmus and cadaver when they are installed.
- litmus 0.13: `basic` 16/16, `copymove` 13/13, `http` 4/4, `locks` 38/41, `props` 11/14. Every failure is a `PROPPATCH` (dead properties) answered with 501.
- HelixNotes CLI (`helixnotes-cli`, a strict client): two vaults syncing through the server, including edits and deletes in both directions, with every change committed. It exposed a `dav-server` bug (unquoted `getetag` in PROPFIND, so the client saw only weak ETags and refused to delete); the server now quotes them. Nothing here depends on HelixNotes.

## Documents

| File | Purpose |
|------|---------|
| `AGENTS.md` | Read first. Ground rules and state for any agent working here. |
| `docs/CONTEXT.md` | Why this exists, how the idea arose, prior art considered. |
| `docs/DESIGN.md` | The approved design, plus an "As built" section. |
| `docs/CONCERNS.md` | Hard parts and edge cases, with the handling chosen. |
| `docs/OPEN-QUESTIONS.md` | Questions asked and answered. |
| `docs/superpowers/plans/2026-10-28-davgit-v1.md` | The implementation plan. |

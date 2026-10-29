# davgit

**A WebDAV server where every save is a git commit.**

Point a notes app, rclone, or your file manager at a folder over WebDAV. Behind it sits an ordinary git repository. Every upload, delete and move becomes a commit, so "what did this note say last Tuesday?" is a `git log` away, and "undo that" is a `git revert`.

```
$ git -C /srv/notes log --oneline
3f2a9c1 davgit: update projects/pizza-night.md
b81d0e4 davgit: move drafts/idea.md -> projects/idea.md
9c07e55 davgit: delete scratch/old-list.md
41de6a0 davgit: 3 changes
```

Want an off-site copy? Give it a remote and it pushes on a timer. If something looks wrong it stops and emails you, instead of guessing.

*(davgit is a working name and may change.)*

## Why this exists

Plain files are a good home for notes, and a git history is a good safety net for them. Most WebDAV servers don't version anything, and "put a file watcher on a git folder" gets crude fast: half-written files get committed, commit timing is a coin flip, and nobody tells you when a push fails. davgit is the small, careful version of that idea: one repo, one user, one binary.

It makes no assumptions about your client. It has been run against rclone, the litmus conformance suite and a strict notes-sync client (results at the bottom).

## Try it

```sh
cargo build --release
cp config.example.toml davgit.toml          # set repo_path, listen, auth
printf '%s' 'choose-a-password' > password  # the file auth.password_file points at
target/release/davgit --config davgit.toml
```

Point a WebDAV client at `http://127.0.0.1:8080/` and log in with the config's username and password. `curl http://127.0.0.1:8080/healthz` tells you how it's doing. `davgit --help` lists the options, and `config.example.toml` documents every setting.

There's no `git init` step: if `repo_path` has no repository, davgit makes one.

### Docker

```sh
mkdir -p data secrets && sudo chown 10001 data
printf '%s' 'choose-a-password' > secrets/password
cp config.docker.example.toml davgit.toml   # edit
docker compose up -d
```

Details are in `compose.yaml`. The image doesn't do TLS; put a reverse proxy (Caddy, nginx, Traefik) in front. Basic auth over plain HTTP is not something to expose to the internet.

## What it's picky about

- **No half-written files.** An upload goes to a temp file inside `.git` and is renamed into place when it's complete. Readers and commits only ever see whole files, and a dropped connection leaves nothing behind.
- **`.git` doesn't exist, as far as clients can tell.** 404 on every method, and nothing can be created, moved or copied into it. Symlinks are never followed.
- **It never rewrites your tree behind your back.** No rebase, no merge, no force-push, no fast-forwarding the working tree. It does not try to be clever about conflicts.
- **It won't guess on first start.** Ambiguous situations get a question, not a coin flip (see below).
- **Git behaves as the config says.** The host's system and global git config is ignored, and `[git.config]` entries (say, `"core.autocrlf" = "false"`) are written to the repo at every start. Your setup doesn't depend on whose `~/.gitconfig` it happens to run under.

## When things go wrong

| Situation | What davgit does |
|-----------|------------------|
| The remote has commits it doesn't have | Fetches only to look, pauses pushing, emails you the ahead/behind counts and the commands to fix it. Keeps serving and committing. Resumes on its own once a push is a fast-forward again. |
| You start a rebase or merge inside its repo | Stands down: skips commits and pushes, answers writes with `503` and `Retry-After`, keeps serving reads. Carries on when you finish or abort. |
| A commit fails | Keeps the change on disk, retries every few seconds, emails you once, and tells you when it recovers. |
| A push fails 3 times in a row | Emails you with a hint for the kind of failure: auth, host key, or network. |
| An upload goes over `max_file_size` (100 MiB by default) | `413`, and the temp file is removed. |
| It finds uncommitted changes at startup (a crash, say) | Commits them, unless a person has a rebase or merge in progress. |
| It's started on the wrong branch with `[push]` configured | Refuses to start, so commits don't land where you didn't mean them to. |

Alerts go out once, then at most once per `notify.rate_limit` (default 24 h) while a problem lasts, followed by a "recovered" notice. Email is the first channel. Adding another means one new `Notifier` implementation and one config section. A failing notifier never blocks serving, committing or pushing.

## How commits work

In the default `per_request` mode, each write is committed as soon as it completes and the HTTP response waits for the commit (`wait_for_commit = true`). Writes that arrive while a commit is running are committed together. In the benchmark, a single-file commit in a 5,000-file tree took about 5 ms.

Prefer tidier history? `debounce` waits for a quiet period, and `interval` commits on a timer. A commit stages only the paths that changed.

## First start

If `repo_path` already holds a repository, davgit uses it as is. If it's empty (and the remote, if any, is empty too), davgit initialises it. Otherwise it needs to know what you want:

- the directory has files but isn't a repository,
- the remote's branch already has history, or
- the local and remote histories are unrelated.

On a terminal it asks. Without one (a container) it prints the choices to stderr, waits 5 seconds so the logs stay readable, and exits with **code 10**. Answer with `--bootstrap clone|init|abort` or `DAVGIT_BOOTSTRAP=clone|init|abort`: `clone` clones the remote, `init` creates a repository and keeps your local files, `abort` exits without touching anything. A container with a restart policy will loop on code 10 until you answer.

Exit codes: 0 clean, 1 error, 2 bad usage or config, 10 needs a bootstrap answer.

## Pushing to a remote (optional)

Add a `[push]` section and davgit pushes the configured branch every `interval`. Leave it out for a local-only server.

**Treat the remote as a write-only backup.** Anything else writing to that branch (another clone, a web edit, a merged pull request, a restore, a force-push) isn't supported. If it happens, davgit pauses pushing and tells you, as above. To sort it out, work in the server's repository with plain git: merge or rebase the remote's changes by hand, or, if the remote is the one that's wrong, `git push --force-with-lease`.

SSH remotes need `ssh_key` and a `known_hosts` file; unknown or changed host keys are refused. Only plain `known_hosts` entries are understood (hashed `|1|` entries are ignored). HTTPS remotes use `token_file`.

Secrets can come from files or from `DAVGIT_PASSWORD`, `DAVGIT_SMTP_PASSWORD`, `DAVGIT_PUSH_TOKEN` and `DAVGIT_PUSH_SSH_PASSPHRASE`, which win over files.

## Housekeeping

- **Run `git gc` now and then.** A cron job is enough. libgit2 can't repack, and `per_request` mode makes a lot of small commits and loose objects.
- Git LFS isn't supported.
- `GET /healthz` needs no login. It returns only the last commit and push times, whether pushing is paused, and whether a person has git state in the repo. It never lists file names.
- Class 2 locking (in memory) is there for macOS Finder and Windows Explorer.

## Sharp edges

- **No dead properties.** `PROPPATCH` returns 501, so modification times set by a client are ignored (git doesn't store them anyway).
- **Case and Unicode.** The server's filesystem is case-sensitive, so a client on a case-insensitive system can create names that differ only in case. Unicode normalisation isn't applied.
- **`LOCK` on a path that doesn't exist** makes the lock but no empty file; the file appears when the client uploads it.
- **Empty directories** exist on disk, but git doesn't track them, so they vanish from history.
- **Recursive `DELETE`** of a folder can produce several commits instead of one.
- **`/healthz` is reserved.** A file named `healthz` at the repository root can't be reached over WebDAV.
- **A client-written `.gitignore` can quietly stop versioning.** Ignored files are stored and served but not committed; davgit only logs a warning.
- **One writer per repository.** No clustering, no failover. Run one instance per repository.
- **SSH push is unit-tested only.** There was no real SSH server in the test run, so try one push before you rely on it.

## How well has it been tested?

- `cargo test`: unit and integration tests, including a local bare repository as the remote for push, divergence and recovery.
- `scripts/smoke.sh`: rclone syncs a tree up and down, compares the result, and checks the commits. It also runs litmus and cadaver if you have them.
- **litmus 0.13:** `basic` 16/16, `copymove` 13/13, `http` 4/4, `locks` 38/41, `props` 11/14. Every failure is a `PROPPATCH` (dead properties) answered with 501.
- **A strict notes-sync client (HelixNotes' CLI):** two vaults syncing through davgit, including edits and deletes in both directions, with every change committed. It only works with strong ETags, and it caught a bug in the WebDAV library davgit is built on (PROPFIND `getetag` came back without quotes, so the client saw weak ETags and refused to delete). davgit now quotes them. Nothing here depends on HelixNotes.

## Under the hood

Rust, `tokio` and `hyper`, the [`dav-server`](https://crates.io/crates/dav-server) crate for the WebDAV protocol, and [`git2`](https://crates.io/crates/git2) (libgit2) for all git work. It never shells out to the `git` binary.

## Documents

| File | Purpose |
|------|---------|
| `AGENTS.md` | Read first. Ground rules and state for any agent working here. |
| `docs/CONTEXT.md` | Why this exists, how the idea arose, prior art considered. |
| `docs/DESIGN.md` | The approved design, plus an "As built" section. |
| `docs/CONCERNS.md` | Hard parts and edge cases, with the handling chosen. |
| `docs/OPEN-QUESTIONS.md` | Questions asked and answered. |
| `docs/superpowers/plans/2026-10-28-davgit-v1.md` | The implementation plan. |

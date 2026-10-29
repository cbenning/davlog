<h1 align="center">davlog</h1>

<p align="center"><strong>A WebDAV server where every save is a git commit.</strong></p>

<p align="center">
  <a href="https://github.com/cbenning/davlog/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/cbenning/davlog/actions/workflows/ci.yml/badge.svg"></a>
  <img alt="License: MIT" src="https://img.shields.io/badge/license-MIT-blue">
  <img alt="Rust" src="https://img.shields.io/badge/Rust-2021-dea584?logo=rust&logoColor=white">
  <img alt="Backed by git" src="https://img.shields.io/badge/backed%20by-git-F05032?logo=git&logoColor=white">
  <img alt="WebDAV" src="https://img.shields.io/badge/WebDAV-class%201%20%26%202-4c8bf5">
  <img alt="Docker ready" src="https://img.shields.io/badge/Docker-ready-2496ED?logo=docker&logoColor=white">
  <img alt="litmus basic, copymove and http suites: 100%" src="https://img.shields.io/badge/litmus-basic%20%C2%B7%20copymove%20%C2%B7%20http%20100%25-brightgreen">
  <img alt="Status: early days" src="https://img.shields.io/badge/status-early%20days-orange">
</p>

<p align="center">
  <a href="#highlights">Highlights</a> &middot;
  <a href="#try-it">Try it</a> &middot;
  <a href="#what-its-picky-about">What it's picky about</a> &middot;
  <a href="#when-things-go-wrong">When things go wrong</a> &middot;
  <a href="#pushing-to-a-remote-optional">Backups</a> &middot;
  <a href="#security-and-hardening">Security</a> &middot;
  <a href="#sharp-edges">Sharp edges</a>
</p>

---

Point a notes app, rclone, or your file manager at a folder over WebDAV. Behind it sits an ordinary git repository. Every upload, delete and move becomes a commit, so "what did this note say last Tuesday?" is a `git log` away, and "undo that" is a `git revert`.

```
 your apps  ──WebDAV──▶  davlog  ──commit──▶  git repo  ──push (optional)──▶  backup remote
```

```
$ git -C /srv/notes log --oneline
3f2a9c1 davlog: update projects/pizza-night.md
b81d0e4 davlog: move drafts/idea.md -> projects/idea.md
9c07e55 davlog: delete scratch/old-list.md
41de6a0 davlog: 3 changes
```

Want an off-site copy? Give it a remote and it pushes on a timer. If something looks wrong it stops and emails you, instead of guessing.

*(davlog is a working name and may change.)*

## Highlights

- 📝 **Every save is a commit.** Uploads, deletes, moves and copies all land in git with readable messages like `davlog: update notes/a.md`. History, diffs and undo are plain git.
- 🧱 **Whole files only.** Uploads are written to a temp file and renamed into place, so nobody, including the committer, ever sees half a file. A dropped connection leaves nothing behind.
- ⚡ **Quick.** A single-file commit took about 5 ms in a 5,000-file tree, and writes that arrive during a commit are committed together.
- 🛟 **Careful with your backup remote.** It pushes on a timer, but never rebases, merges or force-pushes. If the remote changed behind its back, it pauses and tells you.
- 📬 **Tells you when something is wrong.** Email alerts (more channels can be added) with the commands to fix it, rate-limited, and a "recovered" note when it clears.
- 🤝 **Gets out of your way.** Start a rebase or merge inside its repo and it stands down: writes get `503`, reads keep working, and it carries on when you're done.
- 🧰 **Sets itself up.** No `git init` step. On first start it creates the repo, or asks what to do if the situation is ambiguous.
- 🔌 **Plain WebDAV.** No special client needed. Tested with rclone, the litmus suite and a strict notes-sync client. Class 2 locking for Finder and Explorer.
- 🔒 **Locked down.** `.git` and its lookalikes are invisible to clients, symlinks are never followed, passwords are compared in constant time (argon2 hashes supported), and there are limits against slow clients, full disks and floods. See [Security and hardening](#security-and-hardening).
- 🧹 **Looks after its own repository.** It repacks loose objects in the background, without ever deleting an object.
- 📦 **One binary, or Docker.** TOML config, secrets from files or environment, and a `/healthz` endpoint that never lists file names.

## Why this exists

Plain files are a good home for notes, and a git history is a good safety net for them. Most WebDAV servers don't version anything, and "put a file watcher on a git folder" gets crude fast: half-written files get committed, commit timing is a coin flip, and nobody tells you when a push fails. davlog is the small, careful version of that idea: one repo, one user, one binary.

WebDAV sync can also be error-prone. Clients disagree about who changed what, clocks drift, syncs get interrupted halfway, and sometimes a client decides your notes should be deleted or overwritten. Behind davlog that is annoying instead of permanent: every state the server accepted is a commit, so a bad sync can be found in `git log` and put back with `git revert` or `git checkout`. (The history lives in the server's repository, so it is only as safe as that disk. The optional push to a remote is there for exactly that.)

It makes no assumptions about your client. It has been run against rclone, the litmus conformance suite and a strict notes-sync client (results at the bottom).

## Try it

```sh
cargo build --release
cp config.example.toml davlog.toml          # set repo_path, listen, auth
printf '%s' 'choose-a-password' > password  # the file auth.password_file points at
target/release/davlog --config davlog.toml
```

Point a WebDAV client at `http://127.0.0.1:8080/` and log in with the config's username and password. `curl http://127.0.0.1:8080/healthz` tells you how it's doing. `davlog --help` lists the options, and `config.example.toml` documents every setting.

There's no `git init` step: if `repo_path` has no repository, davlog makes one.

### Docker, with HTTPS

```sh
mkdir -p data secrets && sudo chown 10001 data
printf '%s' 'choose-a-long-password' > secrets/password
cp config.docker.example.toml davlog.toml       # edit
DAVLOG_DOMAIN=notes.example.com docker compose -f compose.https.yaml up -d
```

`compose.https.yaml` runs davlog behind [Caddy](https://caddyserver.com/), which gets and renews a Let's Encrypt certificate for `DAVLOG_DOMAIN` by itself and redirects HTTP to HTTPS. Only Caddy publishes ports; davlog is reachable only from inside the compose network. Without `DAVLOG_DOMAIN` it serves `https://localhost` with a certificate from Caddy's local CA, which is good for a first try (`HTTP_PORT` and `HTTPS_PORT` change the published ports if 80 and 443 are not available to you).

The compose file also runs davlog with a read-only root filesystem, no Linux capabilities and `no-new-privileges`, and the `Caddyfile` hides `/healthz` from the outside.

**Use HTTPS.** davlog speaks plain HTTP and Basic auth, so your password crosses the wire in the clear unless TLS is in front of it. Any reverse proxy works (nginx, Traefik, a cloud load balancer); `Caddyfile` is the shortest example. `compose.yaml` is the same container without a proxy, for localhost or a network you trust.

## What it's picky about

- **No half-written files.** An upload goes to a temp file inside `.git` and is renamed into place when it's complete. Readers and commits only ever see whole files, and a dropped connection leaves nothing behind.
- **`.git` doesn't exist, as far as clients can tell.** 404 on every method, and nothing can be created, moved or copied into it. Symlinks are never followed.
- **It never rewrites your tree behind your back.** No rebase, no merge, no force-push, no fast-forwarding the working tree. It does not try to be clever about conflicts.
- **It won't guess on first start.** Ambiguous situations get a question, not a coin flip (see below).
- **Git behaves as the config says.** The host's system and global git config is ignored, and `[git.config]` entries (say, `"core.autocrlf" = "false"`) are written to the repo at every start. Your setup doesn't depend on whose `~/.gitconfig` it happens to run under.

## When things go wrong

| Situation | What davlog does |
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

If `repo_path` already holds a repository, davlog uses it as is. If it's empty (and the remote, if any, is empty too), davlog initialises it. Otherwise it needs to know what you want:

- the directory has files but isn't a repository,
- the remote's branch already has history, or
- the local and remote histories are unrelated.

On a terminal it asks. Without one (a container) it prints the choices to stderr, waits 5 seconds so the logs stay readable, and exits with **code 10**. Answer with `--bootstrap clone|init|abort` or `DAVLOG_BOOTSTRAP=clone|init|abort`: `clone` clones the remote, `init` creates a repository and keeps your local files, `abort` exits without touching anything. A container with a restart policy will loop on code 10 until you answer.

Exit codes: 0 clean, 1 error, 2 bad usage or config, 10 needs a bootstrap answer.

## Pushing to a remote (optional)

Add a `[push]` section and davlog pushes the configured branch every `interval`. Leave it out for a local-only server.

**Treat the remote as a write-only backup.** Anything else writing to that branch (another clone, a web edit, a merged pull request, a restore, a force-push) isn't supported. If it happens, davlog pauses pushing and tells you, as above. To sort it out, work in the server's repository with plain git: merge or rebase the remote's changes by hand, or, if the remote is the one that's wrong, `git push --force-with-lease`.

SSH remotes need `ssh_key` and a `known_hosts` file; unknown or changed host keys are refused. Only plain `known_hosts` entries are understood (hashed `|1|` entries are ignored). HTTPS remotes use `token_file`.

Secrets can come from files or from `DAVLOG_PASSWORD`, `DAVLOG_SMTP_PASSWORD`, `DAVLOG_PUSH_TOKEN` and `DAVLOG_PUSH_SSH_PASSPHRASE`, which win over files.

## Housekeeping

- **Repacking is automatic.** `per_request` mode makes a lot of small commits and loose objects, and libgit2 can't run `git gc`, so davlog packs the repository itself (see [Repository upkeep](#repository-upkeep)).
- Git LFS isn't supported.
- `GET /healthz` needs no login. It returns only the last commit and push times, whether pushing is paused, and whether a person has git state in the repo. It never lists file names.
- Class 2 locking (in memory) is there for macOS Finder and Windows Explorer.

## Security and hardening

### What davlog does for you

- **Passwords are compared in constant time**, and `auth.password_hash` takes an argon2id hash (`printf '%s' 'the password' | davlog hash-password` makes one). Failed logins are logged with the client address, never with what was typed.
- **`.git` is invisible**, along with names that act as `.git` on someone else's checkout (`.GIT`, `.git.`, `GIT~1`, `.git::$INDEX_ALLOCATION`, and names padded with invisible characters). Symlinks are never followed.
- **No destructive git operations.** Nothing in davlog rewrites history, force-pushes or prunes, so someone with stolen WebDAV credentials can delete current files but cannot erase what is already committed.
- **Limits against abuse.** Per-file size, a free-disk floor, an optional repository size cap (writes get `507`, deletes still work), a connection cap, a header-read timeout, a stalled-upload cutoff, `Depth: infinity` refused, and bounded request and PROPFIND sizes. All are in `config.example.toml`.
- **Stored content can't take over the origin.** Every response carries `X-Content-Type-Options: nosniff`, and HTML, XHTML, SVG and XML are served with `Content-Security-Policy: sandbox`, so a stored page opened in a browser cannot run script against your other files.
- **Quiet about itself.** `/healthz` exposes only counters and flags, and secrets never appear in logs or debug output.

### What to do yourself

- **Use HTTPS**, always. `compose.https.yaml` is a working example with Caddy.
- **Keep it off the open internet if you can.** A VPN such as Tailscale or WireGuard, or an IP allowlist at your proxy, removes most of the attack surface in one step.
- **Choose a long random password**, or better, put an argon2 hash in `auth.password_hash`.
- **Slow down password guessing at the proxy.** davlog logs each failed login as `failed login peer=<address>`, which tools like fail2ban can read. Behind a proxy that address is the proxy's, so rate-limit and ban there (Caddy has rate-limit plugins; nginx has `limit_req`). `auth.failed_login_delay` adds a pause before a wrong-password answer. It is off by default, because a delay also holds the connection open while it waits, so guessers can use it to tie up connection slots. Turn it on if you want a simple extra and have `max_connections` headroom.
- **Prefer secret files to environment variables.** `docker inspect` shows a container's environment to anyone who can run it. The compose files use Docker secrets, and config files that hold secrets should be readable only by their owner (`chmod 600`).
- **Give the push credential the least access it needs**: a deploy key or fine-grained token limited to the one backup repository, with write access and nothing else. Keep `known_hosts` current. Give the SMTP account its own app password.
- **Keep the container locked down.** The compose file already runs it as a non-root user with a read-only filesystem, no capabilities and `no-new-privileges`. Consider `mem_limit` and `pids_limit` too (commented examples are in `compose.https.yaml`).
- **Watch the disk.** The `507` limits stop a full disk from taking the machine down, and an alert goes out when they kick in. History only grows, so size the volume for it.
- **Back up the volume as well as the remote.** The history lives on the server's disk. The optional push is one copy; snapshots of the volume are another.
- **Stay current.** Rebuild the image regularly so OpenSSL and libgit2 fixes arrive. The repository has Dependabot for dependencies and pinned base images, and a weekly `cargo audit`. At the time of writing the audit reports one warning, an "unsound" advisory on the `lru` crate that comes in through `dav-server`, with no fixed release yet.
- **Treat the repository like any untrusted one if you clone it elsewhere.** Its contents came from whoever has the WebDAV password. Don't run scripts or hooks from it blindly.

### Repository upkeep

Every commit adds loose objects, and libgit2 can't run `git gc`, so davlog repacks the repository itself (`[maintenance]`, on by default). Every 6 hours it checks, and if there are more than 2,000 loose objects or 20 packs it:

1. writes **all** objects (loose or packed, reachable or not) into one new pack,
2. reads every object back from that pack on its own to prove it is complete,
3. and only then deletes the loose objects and old packs it replaced.

It never prunes: an unreachable object stays in the pack, a pack with a `.keep` file is left alone, and if anything fails before step 3 nothing is deleted. It uses its own repository handle, so commits don't wait for it. `git fsck --full` is clean on a repacked repository and plain `git gc` still works on it.

What it does **not** do is shrink history. A file you committed and later deleted stays in the repository forever, and so does a secret that was ever uploaded. Removing those means rewriting history, which davlog will never do for you: stop it, run a tool like `git filter-repo` on the repository, and start it again.

## Sharp edges

- **No dead properties.** `PROPPATCH` returns 501, so modification times set by a client are ignored (git doesn't store them anyway).
- **Case and Unicode.** The server's filesystem is case-sensitive, so a client on a case-insensitive system can create names that differ only in case. Unicode normalisation isn't applied.
- **`LOCK` on a path that doesn't exist** makes the lock but no empty file; the file appears when the client uploads it.
- **Empty directories** exist on disk, but git doesn't track them, so they vanish from history.
- **Recursive `DELETE`** of a folder can produce several commits instead of one.
- **`/healthz` is reserved.** A file named `healthz` at the repository root can't be reached over WebDAV.
- **A client-written `.gitignore` can quietly stop versioning.** Ignored files are stored and served but not committed; davlog only logs a warning.
- **One writer per repository.** No clustering, no failover. Run one instance per repository.
- **SSH push is unit-tested only.** There was no real SSH server in the test run, so try one push before you rely on it.

## How well has it been tested?

- `cargo test`: unit and integration tests, including a local bare repository as the remote for push, divergence and recovery.
- `scripts/smoke.sh`: rclone syncs a tree up and down, compares the result, and checks the commits. It also runs litmus and cadaver if you have them.
- **litmus 0.13:** `basic` 16/16, `copymove` 13/13, `http` 4/4, `locks` 38/41, `props` 11/14. Every failure is a `PROPPATCH` (dead properties) answered with 501.
- **A strict notes-sync client (HelixNotes' CLI):** two vaults syncing through davlog, including edits and deletes in both directions, with every change committed. It only works with strong ETags, and it caught a bug in the WebDAV library davlog is built on (PROPFIND `getetag` came back without quotes, so the client saw weak ETags and refused to delete). davlog now quotes them. Nothing here depends on HelixNotes.

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
| `docs/superpowers/plans/2026-10-28-davlog-v1.md` | The implementation plan. |

## License

[MIT](LICENSE)

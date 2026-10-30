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
  <a href="#quick-start">Quick start</a> &middot;
  <a href="#when-things-go-wrong">When things go wrong</a> &middot;
  <a href="#pushing-to-a-remote-optional">Backups</a> &middot;
  <a href="#security-and-hardening">Security</a> &middot;
  <a href="#known-limits">Known limits</a>
</p>

---

davlog is a WebDAV server that keeps your files in a git repository. Every upload, delete and move becomes a commit, so anything you sync over WebDAV (notes, mostly) gets a history and an undo button, and plain git does the work.

```
 your apps  ──WebDAV──▶  davlog  ──commit──▶  git repo  ──push (optional)──▶  backup remote
```

```
$ git log --oneline
3f2a9c1 davlog: update projects/pizza-night.md
b81d0e4 davlog: move drafts/idea.md -> projects/idea.md
9c07e55 davlog: delete scratch/old-list.md
41de6a0 davlog: 3 changes
```

If you give it a remote, it also pushes there on a timer for an off-site copy. When something goes wrong it emails you.

## Highlights

- 📝 **Every save is a commit.** Uploads, deletes, moves and copies each become a commit with a readable message like `davlog: update notes/a.md`. History, diffs and undo are normal git.
- 🧱 **No half-written files.** Uploads go to a temp file and are renamed into place when they finish. A dropped connection leaves nothing behind.
- ⚡ **Fast enough that you won't notice.** A single-file commit took about 5 ms in a 5,000-file repo. Writes that arrive while a commit is running are committed together.
- 🛟 **Safe with your backup remote.** davlog pushes on a timer. It never rebases, merges or force-pushes. If the remote has changes it doesn't know about, it stops pushing and emails you.
- 📬 **Email alerts.** When something goes wrong you get one email with the git commands to fix it, a reminder once a day while it lasts, and a note when it clears. Other channels can be added.
- 🤝 **Steps aside when you use git.** If you start a rebase or merge in its repository, writes get a `503` until you finish. Reads keep working.
- 🧰 **Sets itself up.** There's no `git init` step. On first start it creates the repository, or asks what to do if the right answer isn't obvious.
- 🔌 **Plain WebDAV.** Tested with rclone and a strict notes-sync client, and it passes the litmus suites for basics, copy/move and HTTP. Locking is there for Finder and Explorer.
- 🔒 **Locked down.** `.git` and names that imitate it are hidden, symlinks aren't followed, and there are limits for slow clients, full disks and floods. See [Security and hardening](#security-and-hardening).
- 🧹 **Tidies its own repository.** It packs loose objects in the background and never deletes anything.
- 📦 **Docker first.** Compose files with automatic HTTPS through Caddy. It's also a single binary with a TOML config.

## Why this exists

Notes are just files, and git is a good safety net for files. Most WebDAV servers don't keep any history. The usual workaround is a file watcher that commits a git folder, which tends to commit half-written files, picks commit times at random, and says nothing when a push fails. davlog is a small server built to do that job properly: one repository, one user, one binary.

WebDAV sync also goes wrong sometimes. Two clients disagree about what changed, a clock is off, a sync stops halfway, or a client decides to delete or overwrite your notes. With davlog every state it accepted is a commit, so you can find the bad sync in `git log` and put things back with `git revert` or `git checkout`. The history lives on the server's disk, so it's only as safe as that disk. That's what the optional push to a remote is for.

It doesn't assume anything about your client.

## Quick start

Save this as `docker-compose.yml`:

```yaml
name: davlog

services:
  davlog:
    image: ghcr.io/cbenning/davlog:latest
    restart: unless-stopped
    ports:
      - "127.0.0.1:8080:8080"
    environment:
      DAVLOG_PASSWORD: change-me
      DAVLOG_BOOTSTRAP: init
    volumes:
      - data:/data
    configs:
      - source: davlog.toml
        target: /etc/davlog/davlog.toml

volumes:
  data:

configs:
  davlog.toml:
    content: |
      [server]
      listen = "0.0.0.0:8080"
      repo_path = "/data"
      [auth]
      username = "notes"
```

Then run `docker compose up -d`.

davlog is now at `http://localhost:8080/`. Log in with username `notes` and the password you set (`change-me` above). Point any WebDAV client at it; for example, with rclone:

```sh
rclone ls :webdav: --webdav-url=http://localhost:8080 --webdav-user=notes \
  --webdav-pass="$(rclone obscure change-me)" --webdav-vendor=other
```

Your files are in a git repository inside the `data` volume. To see the history:

```sh
docker run --rm -v davlog_data:/data alpine/git -c safe.directory=/data -C /data log --oneline
```

This is plain HTTP on localhost only, which is fine for trying it out. For a server other people can reach, put HTTPS in front of it (see below). `docker compose down` stops it, and `docker compose down -v` also deletes your files.

### Keep the files in a normal folder

`compose.yaml` in this repo does the same thing with the repository in `./data` on your machine, owned by you, so you can use git on it directly:

```sh
git clone https://github.com/cbenning/davlog.git && cd davlog
mkdir -p data
printf 'DAVLOG_UID=%s\nDAVLOG_GID=%s\n' "$(id -u)" "$(id -g)" > .env   # so ./data is yours
docker compose up -d
git -C data log --oneline
```

The login is `notes` / `notes` (set `DAVLOG_PASSWORD=something-else` to change it). To pin a release instead of `latest`, set `DAVLOG_VERSION=1.0.0-RC3`. To build the image from your checkout, add `-f compose.dev.yaml`: `docker compose -f compose.yaml -f compose.dev.yaml up -d --build`.

### Connect HelixNotes

In the app's WebDAV settings use the same URL, username and password. For the CLI:

```sh
export HELIXNOTES_WEBDAV_URL=http://localhost:8080/
export HELIXNOTES_WEBDAV_USERNAME=notes
export HELIXNOTES_WEBDAV_PASSWORD_FILE=~/.config/helixnotes/password   # a file containing your password
export HELIXNOTES_VAULT=~/notes
helixnotes-cli sync
```

Sync a vault, change or delete a note, sync again, and look at the history to see each change as a commit.

### Run it for real, with HTTPS

This sets up davlog behind [Caddy](https://caddyserver.com/), which gets and renews a Let's Encrypt certificate for you. Run it from a checkout of this repository (`git clone https://github.com/cbenning/davlog.git && cd davlog`).

```sh
mkdir -p data secrets
sudo chown 10001 data                          # davlog runs as uid 10001 inside the container
printf '%s' 'a-long-password' > secrets/password
cp config.docker.example.toml davlog.toml      # set the username if you like

DAVLOG_DOMAIN=notes.example.com docker compose -f compose.https.yaml up -d
```

Point a WebDAV client at `https://notes.example.com/` and log in with the username from `davlog.toml` and the password you put in `secrets/password`.

Your files live in `./data`, which is the git repository. To look at the history from the host, run git as the container's user:

```sh
sudo -u '#10001' git -C data log --oneline
```

A few notes:

- Without `DAVLOG_DOMAIN` it serves `https://localhost` with a certificate from Caddy's own local CA. That's fine for trying out the HTTPS setup. If ports 80 and 443 aren't available to you, set `HTTP_PORT` and `HTTPS_PORT`.
- Only Caddy publishes ports. davlog itself is reachable only from inside the compose network, runs with a read-only filesystem and no extra Linux capabilities, and the `Caddyfile` hides `/healthz` from the outside.
- Don't expose davlog directly, as `compose.yaml` does on localhost. It speaks plain HTTP with Basic auth, so the password would cross the network unencrypted. Any reverse proxy works (nginx, Traefik, a cloud load balancer). `Caddyfile` is just the shortest example.
- To use a password hash instead of a password file, make one with `printf '%s' 'a-long-password' | docker compose -f compose.https.yaml run --rm -T davlog hash-password` and put it in `davlog.toml` as `password_hash`.
- `config.example.toml` documents every setting.

### Without Docker

```sh
cargo build --release
cp config.example.toml davlog.toml        # set repo_path, listen and auth
printf '%s' 'a-long-password' > password  # the file auth.password_file points at
target/release/davlog --config davlog.toml
```

Then point a client at `http://127.0.0.1:8080/`. `curl http://127.0.0.1:8080/healthz` shows how it's doing, and `davlog --help` lists the options. Put a TLS proxy in front of it for anything beyond localhost.

### First start

You don't run `git init`. If the data directory has no repository, davlog creates one. If it already has one, davlog uses it as is. If it's empty and the remote (if you set one) is empty too, it initialises the repository.

In three cases it can't tell what you want, so it asks:

- the directory has files in it but isn't a repository,
- the remote's branch already has history, or
- the local and remote histories are unrelated.

On a terminal it prompts you. In a container there's no terminal, so it prints the choices, waits 5 seconds so the logs stay readable, and exits with **code 10**. Answer with `DAVLOG_BOOTSTRAP=clone|init|abort` (or `--bootstrap` when running the binary): `clone` clones the remote, `init` creates a repository and keeps your files, and `abort` exits without changing anything. A container with a restart policy will keep restarting on code 10 until you answer. You can also answer interactively with `docker compose -f compose.https.yaml run --rm davlog`.

Exit codes: 0 clean, 1 error, 2 bad usage or config, 10 needs a bootstrap answer.

## When things go wrong

| Situation | What davlog does |
|-----------|------------------|
| The remote has commits it doesn't have | Fetches only to look, stops pushing, and emails you the ahead/behind counts and the commands to fix it. Keeps serving and committing. Starts pushing again on its own once a push would be a fast-forward. |
| You start a rebase or merge inside its repo | Stops committing and pushing, answers writes with `503` and `Retry-After`, and keeps serving reads. Carries on when you finish or abort. |
| A commit fails | Keeps the change on disk, retries every few seconds, emails you once, and tells you when it recovers. |
| A push fails 3 times in a row | Emails you, with a hint for the kind of failure: auth, host key, or network. |
| An upload goes over `max_file_size` (100 MiB by default) | Returns `413` and removes the temp file. |
| It finds uncommitted changes at startup (after a crash, say) | Commits them, unless you have a rebase or merge in progress. |
| It starts on the wrong branch with `[push]` configured | Refuses to start, so commits don't land on a branch you didn't mean. |

An alert goes out once, then at most once per `notify.rate_limit` (24 hours by default) while the problem lasts, and a "recovered" note follows when it clears. Email is the only channel so far. Adding another takes one new `Notifier` implementation and one config section. A failing notifier never blocks serving, committing or pushing.

## How commits work

In the default `per_request` mode, each write is committed as soon as it finishes, and the HTTP response waits for the commit (`wait_for_commit = true`). Writes that arrive while a commit is running are committed together. A commit stages only the paths that changed.

If you'd rather have tidier history, `debounce` waits for a quiet period and `interval` commits on a timer.

## Pushing to a remote (optional)

Add a `[push]` section and davlog pushes the configured branch every `interval`. Leave it out and davlog just keeps the local repository.

**Treat the remote as a write-only backup.** Anything else writing to that branch (another clone, an edit on the web, a merged pull request, a restore, a force-push) isn't supported. When it happens, davlog stops pushing and emails you, and you fix it by hand with plain git in the server's repository: merge or rebase the remote's changes, or, if the remote is the one that's wrong, `git push --force-with-lease`. With the Docker setup the repository is `./data`, so run those commands as `sudo -u '#10001' git -C data ...`. While your rebase or merge is in progress, davlog stays out of the way, and it picks up again when you're done.

davlog doesn't branch or merge for you. Starting a side branch when the remote diverges was considered and left out for now, because it moves your history somewhere you didn't ask for.

SSH remotes need `ssh_key` and a `known_hosts` file, and unknown or changed host keys are refused. Only plain `known_hosts` entries are understood; hashed `|1|` entries are ignored. The file must list every host key type the server might present (a server can offer ed25519, ECDSA or RSA), not just one. For GitHub, this writes all three from GitHub's own API:

```sh
gh api meta --jq '.ssh_keys[]' | sed 's/^/github.com /' > known_hosts
```

Use a deploy key limited to the one backup repository for `ssh_key`: `ssh-keygen -t ed25519 -N "" -f davlog_ssh_key`, then add the `.pub` file to the repo's deploy keys with write access. HTTPS remotes use `token_file`.

Secrets can come from files or from `DAVLOG_PASSWORD`, `DAVLOG_SMTP_PASSWORD`, `DAVLOG_PUSH_TOKEN` and `DAVLOG_PUSH_SSH_PASSPHRASE`. The environment variables win over files.

## Other things worth knowing

- **Git settings come from davlog's config.** The host's system and global git config is ignored. Put what you need under `[git.config]` (for example `"core.autocrlf" = "false"`) and davlog writes it to the repository's config on every start.
- **Repacking is automatic.** `per_request` mode makes a lot of small commits and loose objects, and libgit2 can't run `git gc`, so davlog packs the repository itself. See [Repository upkeep](#repository-upkeep).
- `GET /healthz` needs no login. It returns the last commit and push times, whether pushing is paused, and whether someone has a rebase or merge going in the repo. It never lists file names.
- Locking (in memory) is there for macOS Finder and Windows Explorer.
- Git LFS isn't supported.

## Security and hardening

### What davlog does for you

- **Passwords are compared in constant time**, and `auth.password_hash` takes an argon2id hash. Failed logins are logged with the client address, never with what was typed.
- **`.git` is hidden**, along with names that act as `.git` on someone else's checkout (`.GIT`, `.git.`, `GIT~1`, `.git::$INDEX_ALLOCATION`, and names padded with invisible characters). Symlinks are never followed.
- **No destructive git operations.** Nothing in davlog rewrites history, force-pushes or prunes. Someone with stolen WebDAV credentials can delete your current files, but can't erase what's already committed.
- **Limits against abuse.** Per-file size, a free-disk floor, an optional cap on repository size (writes get `507`, deletes still work), a connection limit, a header-read timeout, a cutoff for stalled uploads, `Depth: infinity` refused, and bounds on request and PROPFIND sizes. They're all in `config.example.toml`.
- **Stored files can't take over the origin.** Every response carries `X-Content-Type-Options: nosniff`, and HTML, XHTML, SVG and XML are served with `Content-Security-Policy: sandbox`, so a page someone stored can't run script against your other files when opened in a browser.
- **Secrets stay out of logs and debug output**, and `/healthz` only exposes counters and flags.

### What to do yourself

- **Use HTTPS.** `compose.https.yaml` is a working setup.
- **Keep it off the open internet if you can.** A VPN such as Tailscale or WireGuard, or an IP allowlist at your proxy, removes most of the attack surface in one step.
- **Use a long random password**, or better, an argon2 hash in `auth.password_hash`.
- **Slow down password guessing at the proxy.** davlog logs each failed login as `failed login peer=<address>`, which tools like fail2ban can read. Behind a proxy that address is the proxy's, so rate-limit and ban there (Caddy has rate-limit plugins, nginx has `limit_req`). `auth.failed_login_delay` adds a pause before a wrong-password answer. It's off by default, because a delay also keeps the connection open while it waits, and a flood of guesses could use up the connection slots. Turn it on if you want a simple extra and have `max_connections` to spare.
- **Prefer secret files to environment variables.** `docker inspect` shows a container's environment to anyone who can run it. The compose files use Docker secrets. If you have config files with secrets in them, make them readable only by their owner (`chmod 600`).
- **Give the push credential as little access as possible.** A deploy key or fine-grained token for the one backup repository, with write access and nothing else. Keep `known_hosts` current, and give the SMTP account its own app password.
- **Keep the container locked down.** The compose file runs it as a non-root user with a read-only filesystem, no capabilities and `no-new-privileges`. Consider `mem_limit` and `pids_limit` as well; there are commented examples in `compose.https.yaml`.
- **Watch the disk.** The `507` limits stop a full disk from taking the machine down, and an alert goes out when they kick in. History only grows, so size the volume with that in mind.
- **Back up the volume, not just the remote.** The history lives on the server's disk. The push is one copy, and volume snapshots are another.
- **Keep it updated.** Rebuild the image now and then so OpenSSL and libgit2 fixes arrive. The repository uses Dependabot, pins its base images, and runs `cargo audit` weekly. Right now the audit shows one warning, an "unsound" advisory on the `lru` crate that comes in through `dav-server`. There's no fixed release yet.
- **Be careful if you clone the repository elsewhere.** Its contents came from whoever has the WebDAV password, so don't run scripts or hooks from it without looking.

### Repository upkeep

Every commit adds loose objects, and libgit2 can't run `git gc`, so davlog repacks the repository itself (`[maintenance]`, on by default). Every 6 hours it checks, and if there are more than 2,000 loose objects or 20 packs, it:

1. writes all objects (loose or packed, reachable or not) into one new pack,
2. reads every object back from that pack on its own, to check the pack is complete,
3. and only then deletes the loose objects and old packs the new one replaces.

It never prunes. Unreachable objects stay in the pack, a pack with a `.keep` file is left alone, and if anything fails before step 3, nothing is deleted. It uses its own repository handle, so commits don't wait for it. `git fsck --full` is clean on a repacked repository, and plain `git gc` still works on it.

What it doesn't do is shrink history. A file you committed and later deleted stays in the repository, and so does a secret that was ever uploaded. Removing those means rewriting history, and davlog won't do that for you. Stop it, run a tool like `git filter-repo` on the repository, and start it again.

## Known limits

- **No dead properties.** `PROPPATCH` returns 501, so modification times set by a client are ignored. Git doesn't store them anyway.
- **Case and Unicode.** The server's filesystem is case-sensitive, so a client on a case-insensitive system can create names that differ only in case. Unicode normalisation isn't applied.
- **`LOCK` on a path that doesn't exist** creates the lock but no empty file. The file appears when the client uploads it.
- **Empty directories** exist on disk, but git doesn't track them, so they don't show up in history.
- **Deleting a folder** can produce several commits instead of one.
- **`/healthz` is reserved.** A file named `healthz` at the top of the repository can't be reached over WebDAV.
- **A client-written `.gitignore` can stop files being versioned.** Ignored files are stored and served but not committed, and davlog only logs a warning.
- **Don't edit files in the data directory by hand while davlog is running.** It commits what it finds, and a file that is rewritten in place while davlog reads it can crash the process. Changes made over WebDAV are always safe, and so is stopping davlog first.
- **One writer per repository.** There's no clustering or failover. Run one instance per repository.
- **SSH push has only been run against GitHub**, with a deploy key. Other SSH servers should work but haven't been tried.

## Testing

- `cargo test` runs the unit and integration tests, including a local bare repository standing in for the remote to cover push, divergence and recovery.
- `scripts/smoke.sh` has rclone sync a tree up and down, compares the result, and checks the commits. It also runs litmus and cadaver if you have them installed.
- **litmus 0.13:** `basic` 16/16, `copymove` 13/13, `http` 4/4, `locks` 38/41, `props` 11/14. Every failure is a `PROPPATCH` (dead properties) answered with 501.
- **HelixNotes' CLI**, a notes-sync client that checks ETags strictly: two vaults synced through davlog, with edits and deletes in both directions and every change committed. It found a bug in the WebDAV library davlog uses: PROPFIND returned `getetag` without quotes, so the client saw only weak ETags and refused to delete. davlog now adds the quotes. Nothing in davlog depends on HelixNotes.

## Built with

Rust, `tokio` and `hyper`, the [`dav-server`](https://crates.io/crates/dav-server) crate for the WebDAV protocol, and [`git2`](https://crates.io/crates/git2) (libgit2) for all git work. davlog never runs the `git` binary.

## License

[MIT](LICENSE)

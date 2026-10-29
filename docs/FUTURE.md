# Future features

Ideas that were discussed and deliberately left out of v1. Nothing here is promised. Each entry says why it was left out, so the next person does not have to rediscover it.

## Conflict branch (`on_conflict = "branch"`)

**Status:** designed, liked by the owner, parked on 2026-10-28 when push handling was simplified to "pause and alert".

**Why it was left out:** v1 treats the remote as a write-only backup and pauses pushing when it diverges. That needed no rebase, no branch handling and no extra state. A conflict branch keeps off-site backup running through a divergence, but it moves the server's history to a branch the operator did not ask for, so it should be opt-in and needs a few decisions first (below).

**How it would work.** The configured `branch` (say `main`) is the home branch. The conflict branch is a detour from it.

On a rejected push:

1. Fetch for diagnosis, as today. No rebase or merge.
2. Create `<conflict_prefix><timestamp>` at the current HEAD (default prefix `davlog/conflict-`, for example `davlog/conflict-20261028T1412`). It holds every local commit.
3. Point HEAD at the new branch without a checkout. The working tree is untouched, so clients see no change and writes do not block.
4. Push the new branch. A new name cannot be rejected as non-fast-forward, so no force is needed.
5. Alert: the server is now on the conflict branch, merge it into the home branch (with the commands).
6. Leave local `main` and `origin/main` where they are.

While on the conflict branch, commits and pushes continue there, so the backup keeps working. The server never deletes a conflict branch. If a second conflict happens on it (unlikely, nothing else writes there), fall back to pausing.

**Getting back to the home branch.** HEAD (`.git/HEAD`, on the persistent volume) is the only state. There is no state file.

- *Offline merge:* the operator stops the service, runs `git checkout main && git merge --no-ff <conflict branch>`, resolves, pushes, and starts the service. At startup, if HEAD is a conflict branch and its tip is an ancestor of the home branch, the server commits any dirty tree first, takes the write lock, checks out the home branch, and fast-forwards local `main` to `origin/main` if that is safe. If the operator merged inside the server's repository, HEAD is already the home branch and nothing special happens.
- *Online merge (optional):* on each push tick the server fetches and runs the same ancestor check. When the conflict branch has been merged on the remote, it switches back without a restart and sends a "recovered" notice. This costs one extra write-lock window.
- *Not merged yet:* keep serving on the conflict branch and send reminders at the alert rate limit. Never refuse to start. Availability and data safety come first.

**Safety rules:**

- Never switch away from a branch whose commits are not in the home branch's history.
- Never move local `main` unless its tip is an ancestor of `origin/main`. Otherwise refuse, alert, and stay on the conflict branch.
- Awareness without a state file: at startup and on each push tick, list local and remote branches matching the prefix. For each one whose tip is not an ancestor of the home branch, send an "unmerged conflict branch exists" alert (rate limited). If HEAD is on it, keep serving. If HEAD is not on it (the operator ran `git checkout main` by hand), do not switch to it; the operator decides.

**Merge policy.** Conflict branches must be merged back with a real merge commit (`git merge --no-ff`). A squash merge or a forge's "rebase and merge" rewrites hashes, so the branch tip is never an ancestor and it looks unmerged forever. The server would then stay on the conflict branch and keep reminding. Recovery is manual: delete the conflict branch (local and remote), or restart with an override flag such as `--return-to-branch`. Nothing is lost. A tree-equality fallback was considered and rejected as fragile.

**Open decisions:**

- Default for `on_conflict`: `pause` (leaning) or `branch`.
- Whether to switch back online when a remote merge is seen, or only at startup.
- Whether the alert should also open a pull request (probably not).

**Tests it would need:**

- A conflict creates the branch, moves HEAD without changing the working tree, pushes it, and sends one alert with the merge commands.
- Commits and pushes continue on the conflict branch.
- Restart while on a conflict branch stays on it.
- Restart after a real merge commit returns to the home branch, commits a dirty tree first, and fast-forwards local `main`.
- Restart after a manual `git checkout main` with an unmerged conflict branch alerts and does not switch.
- Online merge on the remote is detected on the next tick.
- A squash-merged conflict branch is never treated as merged.
- Local `main` that is not an ancestor of `origin/main` is never moved.
- A second conflict while on a conflict branch falls back to pausing.
- The server never deletes a conflict branch.

## Hot failover

An active instance plus a standby that fast-forwards from the remote, with the old active node fenced before the standby takes over. If both accept writes the pushes diverge, and the existing divergent-remote rule pauses pushing and alerts, so the failure is safe and loud. Lock state is in memory and is lost on failover. Not planned: the project is single-writer per repository, and sharding by repository is the scaling path.

## Automatic rebase

For setups where other people legitimately push to the remote branch. It would bring conflicts, an extra write-lock window, and abort-on-error handling, and it contradicts the "remote is a write-only backup" rule. Only worth doing if that rule changes.

## Dead properties (`PROPPATCH`)

`PROPPATCH` returns 501 today, which accounts for every litmus failure (`props` 11/14, `locks` 38/41). Supporting it means storing dead properties somewhere, probably a side file inside `.git`. No client the project has met needs it, so it is only worth doing for one that does.

## An arm64 Docker image

The release workflow publishes linux/amd64 only. Building arm64 under QEMU on a GitHub runner takes hours, because the build compiles about 230 crates plus libgit2, libssh2 and OpenSSL. Options: build each platform on its own native runner (`ubuntu-24.04-arm`, free for public repositories) and merge the manifests, or cross-compile arm64 on amd64 in the Dockerfile.

## Smaller ideas

- More alert channels: webhook, ntfy, Matrix, a command hook. The `Notifier` trait is ready for them.
- Apply the `failed login peer=<address>` log line to a real client address when running behind a proxy (a trusted `X-Forwarded-For` setting).
- A `track_empty_dirs` setting that writes a `.gitkeep` so empty folders show up in history.
- One commit per request for a recursive DELETE or COPY, by holding changes until the request finishes.
- Test SSH push against a real SSH server in CI.

# Open questions

## Answered by the owner (2026-10-28)

- **Alert events:** any problem, including merge conflicts and push issues.
- **Failure behaviour:** on error, keep committing locally. Only pushing is paused.
- **Per-request commit timing:** configurable, default is to wait for the commit.
- **Initial repo:** the server owns the repo. Initial content and first commit come through WebDAV, no manual git setup.
- **Empty directories:** builder's call.
- **Max file size:** supported and configurable.
- **Resuming pushes after a conflict:** confirmed. The server retries on every push tick and sends a "recovered" notice when it works.
- **Bootstrap against an existing remote:** the service asks the operator at start (confirmed: prompt on a TTY, otherwise log to the console and exit with a flag or env var to answer it; deployment is likely Docker).
- **Name:** `davgit` for now. The owner likes it. Still keep it easy to change.
- **Original requirement list:** nothing further. "Backed by git" was the only item.

These answers are folded into `DESIGN.md` and `CONCERNS.md`.

## Answered by the owner (later the same day, 2026-10-28)

- **Periodic push:** optional. No `[push]` section means local-only.
- **`gix` status:** checked. Not usable for rebase or full push, so `git2` stays.
- **Remote divergence:** the remote is a write-only backup, and concurrent writes to it are explicitly unsupported. On a rejected push the server does something sane and safe: fetch for diagnosis, pause pushing, alert, keep serving. No rebase, no merge, no conflict branch. This supersedes the earlier "fetch, rebase, retry" decision and the `on_conflict` idea. See `DESIGN.md` "Divergent remote".
- **Conflict branch:** liked the idea, but parked under "Possible later" in `DESIGN.md`. If it returns: merge conflict branches back with a real merge commit, never a squash.
- **Scaling:** out of scope. Single writer per repo. Parallelism in answering requests should be configurable (`worker_threads`). Hot failover is a possible later option, not now.

These answers are folded into `DESIGN.md` and `CONCERNS.md`.

- **Default commit mode:** `per_request` (with group commit), unless the commit-cost benchmark shows it is too slow. Then `debounce`.
- **Human working in the repo:** mutating requests return 503 with `Retry-After` while a rebase or merge is in progress. Reads continue.

## Still needs the owner

Nothing blocking. The commit-cost benchmark (see "Left to the builder" in `DESIGN.md`) may reopen the default commit mode.

The owner has not yet reviewed the full written design (`DESIGN.md`), and the independent builder check has not been run.

## Technical, settled by the builder (mark as the builder's call)

- ~~`git2` versus `gix`~~: settled 2026-10-28, `git2`. `gix` lacks rebase and has partial push. Re-check in a few months.
- `dav-server` versus a custom WebDAV implementation, once its filesystem trait is evaluated against the hide and atomic-write requirements.
- Empty directory handling (suggested default in `DESIGN.md`).
- Default value for `max_file_size` and how it is enforced for chunked uploads.
- Commit message template details and default.
- Log format, health endpoint shape, packaging.

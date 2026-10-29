# Concerns and proposed handling

Each concern was raised when scoping the project. The owner said to work through them together. They have been discussed only at a high level so far, so the handling below is the previous agent's proposal unless stated.

1. **Concurrent git operations.** Two commits at once fail with an `index.lock` error, and a commit during a rebase corrupts state. Use one mutex around all repo access. libgit2 is blocking, so run it in `spawn_blocking`.

2. **Hiding `.git`.** Return 404 for any path under `.git` on every method, and refuse PUT, DELETE, MOVE, COPY and MKCOL targets there. Also check on MOVE/COPY destinations. Consider hiding `.gitignore`/`.gitattributes` behaviour as an open question (probably allow them as normal files).

3. **Path traversal and symlinks.** Reject `..` after normalisation. Do not follow symlinks that point outside the root. Decide whether clients may create symlinks (probably not).

4. **Writes during a push.** Revised 2026-10-28: the server never rebases or merges, so fetch and push do not change the working tree and writes never need to block on them. The remaining exclusive windows are short: the commit itself (held on the repo mutex) and graceful shutdown. Concurrent request handling is configurable (`worker_threads`); only git operations are serialized.

5. **Atomic writes.** Write PUT bodies to a temp file in the same directory (or a hidden temp area outside the tracked tree) and rename into place, so a reader or a commit never sees a half-written file. Temp files must never be committed (use a dot-prefixed temp name plus an internal ignore rule, or a temp dir outside the repo on the same filesystem).

6. **Commit during an in-flight upload.** In debounce and interval modes (not the default), a commit may fire while a large upload is still being written. With temp file plus rename the tree only ever contains complete files, so this is safe.

7. **ETag and Last-Modified stability.** Sync clients rely on them. Derive ETag from file size plus mtime (or a content hash if cheap). A rebase or checkout rewrites mtimes, which can make clients re-download files that did not really change. Keep file content hashing as an option if this causes churn.

8. **LOCK/UNLOCK.** macOS Finder and Windows Explorer require class 2 locking. Provide an in-memory lock system. Sync clients usually skip locking.

9. **PROPPATCH and mtime.** Some clients (for example rclone) set modification time through PROPPATCH. Decide whether to honour it (set file mtime) or ignore it. Git does not store mtime, so it will not survive a checkout.

10. **Empty directories.** Git does not track empty directories, but MKCOL creates them. Either accept that empty directories vanish from history (they still exist on disk), or add a placeholder file (`.gitkeep`) automatically. The owner left this to the builder. Suggested default: disk only, optional `.gitkeep` setting.

11. **Case sensitivity, Unicode normalisation, reserved names.** The server's filesystem is case-sensitive on Linux, while a client may be on a case-insensitive system. Not solvable fully; document it.

12. **Large files and binaries.** Git stores every version. A configurable max file size is required (owner confirmed). Reject with 413 and remove the temp file. Document that Git LFS is out of scope for now (libgit2 has no LFS support).

13. **Deletes and MOVE.** A MOVE of a directory is one filesystem rename, and git sees it as deletes plus adds. Add-all staging handles that naturally and rename detection happens at diff time.

14. **Auth.** Basic auth, single user. Store a password hash (argon2) or read a password file. Constant-time comparison. TLS is terminated by a reverse proxy; document this. Rate limit or delay failed logins (Guess).

15. **Git credentials for push.** libgit2 uses credential callbacks. Support SSH key (no agent dependency, optional passphrase) and HTTPS token. Check host key verification: do not blindly accept unknown hosts. Add a `known_hosts` setting.

16. **Humans working in the repo.** Revised 2026-10-28: the server never rebases or merges, so any rebase or merge state (`rebase-merge`, `rebase-apply`, `MERGE_HEAD`) or a foreign `index.lock` in `.git` belongs to a person, for example fixing a divergent remote with the git CLI. Never abort or modify it. Skip commit and push ticks, log why, and resume when the state clears. Mutating WebDAV requests return 503 with `Retry-After` meanwhile (Decided); reads continue.

17. **Startup and shutdown.** At startup verify the repo opens, HEAD is on the configured branch, and the tree state is sane. A dirty tree is committed (or alerted). On SIGTERM, stop accepting writes, flush a final commit, and exit.

18. **Initial repo state.** Decided: the server creates the repo itself when none exists, and initial content comes through WebDAV. See "Repository bootstrap" in `DESIGN.md`: ambiguous cases are put to the operator at first start.

19. **Observability.** Implemented: structured logs to stderr via `tracing` (`RUST_LOG`, ANSI only on a terminal) and an unauthenticated `GET /healthz` that returns only last commit and push time, whether pushing is paused, and whether a person has git state in the repo (never file names).

20. **Alert delivery safety.** Never hold the repo mutex while sending notifications. Time out SMTP. Queue and rate limit. Failures are logged only.

21. **Loose objects and no `gc`.** Resolved 2026-10-29: davlog repacks the repository itself (`Repo::maintain`, `[maintenance]`). It never prunes, so deleted files and old secrets stay in history; removing them needs a history rewrite with an external tool on a stopped instance. See the README, "Repository upkeep".

22. **Divergent remote.** Unsupported by design (the remote is a write-only backup). The server fetches for diagnosis only, pauses pushing, alerts, and resumes when a push is a fast-forward again. See `DESIGN.md` "Divergent remote". The conflict-branch idea is parked under "Possible later"; if it returns, it needs a merge-commit-only policy and a refs scan for unmerged conflict branches.

23. **Group commit and commit cost.** Commit speed is unmeasured. Benchmark before choosing the default commit mode (see `DESIGN.md` "Commit behaviour").

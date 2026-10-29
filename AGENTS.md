# Instructions for agents

## State of the project

- Created 2026-10-28 out of a brainstorming conversation. Design approved 2026-10-28; implementation follows `docs/superpowers/plans/2026-10-28-davlog-v1.md` on branch `feat/v1`.
- `docs/DESIGN.md` is **approved**. The owner confirmed the decisions marked "Decided" in it, including their answers of 2026-10-28 (see `docs/OPEN-QUESTIONS.md`). Everything marked "Guess" is the previous agent's proposal and has not been confirmed.
- A second design pass on 2026-10-28 added: optional push, the `git2` over `gix` decision, and a simpler push policy. The remote is a write-only backup, and a divergent remote is unsupported: the server fetches for diagnosis, pauses pushing, alerts, and never rebases or merges. The conflict-branch idea, automatic rebase and hot failover are parked under "Possible later" in `docs/DESIGN.md`. Scaling is out of scope. Default commit mode is `per_request` unless the benchmark shows it is slow. Mutating requests return 503 while a human has a rebase or merge in progress. No questions block the owner's review.
- The owner approved the written design; the independent "builder check" review of the document was waived by that approval.

## What to do next

1. Read `docs/CONTEXT.md`, then `docs/DESIGN.md`, `docs/CONCERNS.md` and `docs/OPEN-QUESTIONS.md`.
2. Ask the owner to review the draft design and answer the open questions that change what gets built. Do not re-ask things listed as "Decided".
3. Only after the owner approves the design: write an implementation plan (if the superpowers plugin is available, use its `writing-plans` skill), then build.
4. Do not scaffold, add dependencies, or write product code before that approval.

## Owner preferences (carry over)

- Language: Rust.
- Use a real git library (`git2`/libgit2 is the current pick). **Do not shell out to the `git` binary.**
- Keep alerting abstracted behind a trait so more channels can be added. Email is first.
- The server must not assume any particular WebDAV client.
- The project is called `davlog` (renamed from `davgit` on 2026-10-29). It is the real name now, but keep it in as few places as practical (crate name, binary name, config path, user-agent/server strings, mostly in `src/brand.rs`) so a rename stays cheap.
- Owner works in a terse style and likes short, direct answers. Documents and commit messages should still be normal prose.

## Environment notes

- Machine: Fedora (atomic-style, `/var/home/chris`), Rust toolchain via cargo 1.91.1 at time of writing.
- Sibling project for reference only (unaffiliated): `~/workspace/HelixNotes`, a notes app whose CLI (`src-tauri/crates/helix-cli`, `src-tauri/crates/helix-sync`) syncs a folder over WebDAV. It is the likely first real client, which is a good integration test, but nothing here may depend on it.

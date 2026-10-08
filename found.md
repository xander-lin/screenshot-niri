# found.md — Lessons Learned

Verified mistakes and actionable lessons only. Do not fabricate incidents; record new ones promptly.

## 1. Documentation drift: FINDINGS.md/CONTRACT.md claimed long mode was unsupported while the code implemented it

- **Mistake/context**: On 2026-10-08, `FINDINGS.md` (Project Scope, Module Relationships, Architecture Decisions) and `src/CONTRACT.md` (`app`, `stitch`) still described long screenshots as "out of scope", "test-only", and "`--long` remains unsupported", but `src/app.rs` (`run_long`, `LongCaptureWorker`), `src/cli.rs` (`--long`), and the full `src/stitch.rs` + `src/stitch/` port (commits `b289606`..`a8eb251`) implement long capture end-to-end.
- **Root cause**: Documentation was not updated in the same changes that landed the longshot work; the docs had drifted across several milestones.
- **Correction**: Updated the stale FINDINGS.md scope/relationship claims and the `src/CONTRACT.md` `cli`/`app`/`stitch`/`wayland::selection` entries (2026-10-08).
- **Prevention**: Treat code as the source of truth when answering questions or planning work — verify with `grep`/`read` before citing docs. When completing a behavior milestone, update `CONTRACT.md` and `FINDINGS.md` in the same commit.

## 2. Profiling artifacts were committed to the repository

- **Mistake/context**: `perf.data`/`perf.data.old` were committed during performance work and later removed from git by commit `650e4a3` ("Remove perf data, add to gitignore"). The files still exist on disk (now ignored).
- **Root cause**: Profiling output was generated inside the repo before a `.gitignore` rule existed.
- **Prevention**: Write profiling/temp artifacts outside the repo (e.g. `/tmp/opencode`) or ensure ignore rules exist first; review `git status` before committing.

## 3. Removed an "unused" pub method that lib tests still used

- **Mistake/context**: While wiring `push_frame_with_offset` (2026-10-08), `RawStitcher::push_frame_views` looked dead from the bin target's warnings, so it was deleted; `cargo test` then failed because two lib tests call it. The bin-target dead-code warning was misleading: the method was test-only API.
- **Root cause**: Judging API liveness from the bin target's warnings without checking `#[cfg(test)]` users.
- **Correction**: Restored the method behind `#[cfg_attr(not(test), allow(dead_code))]`.
- **Prevention**: Before removing anything flagged dead, grep the whole crate including test modules (`grep -rn "<name>" src/`); prefer `#[cfg_attr(not(test), allow(dead_code))]` for test-only public API over deletion.

## 4. Path-wide `git checkout -- src/` destroyed uncommitted test work during mutation testing

- **Mistake/context**: While mutation-testing the edit-mode feature (2026-10-08), mutations were reverted with `git checkout -- src/` while the new gap-closing tests were still uncommitted; the revert wiped an hour of uncommitted work (later reconstructed from session context and re-committed as `35ac995`). The same mistake was repeated later the same day: `git checkout -- src/wayland/selection.rs` reverted a mutation and also the uncommitted live-selection-box fix (reconstructed, commit `8176f0e`).
- **Root cause**: Mutation experiments and uncommitted feature work shared the working tree; the revert path was wider than the mutations and no `git status` check preceded it. Despite the recorded prevention rule, compound shell commands (`cmd1 && cmd2 ; git checkout -- …`) made the destructive revert a muscle-memory tail.
- **Correction**: Reconstructed and committed both times; subsequent mutations ran on a committed tree.
- **Prevention**: Commit **before** every mutation experiment — no exceptions. Revert mutations only after `git status --short` confirms the tree is clean apart from the mutation, and never chain the revert into a compound command with `;`.

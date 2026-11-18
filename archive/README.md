# archive/

Superseded history kept out of the build, not out of reach.

## `screenshot-rs-rebuild-0.1.0-history.bundle`

A `git bundle` of the retired working tree `/home/work/Work/screenshot-rs-rebuild`
(package version 0.1.0, `PKGBUILD` pkgrel 7), removed on 2026-11-18.

It was a stale clone of this same project: its `niri` branch had **15 commits
that were never pushed** (its HEAD `ca24865` sits ahead of `origin/niri`
`a8eb251`), and none of them are reachable from this repository's history —
which starts at its own root `3b9f7da`. Its working tree was also never
rebuilt from that history: `Cargo.lock` was 11 days newer than every commit, so
the committed sources were not the state that had been tested.

The bundle records the complete history of every ref it carried, including the
`plain` branch (`7bc8175`) and the `v0.1.0-niri` / `v0.1.0-plain` tags. To read
it:

```bash
git clone archive/screenshot-rs-rebuild-0.1.0-history.bundle /tmp/old-tree
git -C /tmp/old-tree log --oneline niri | head
```

Verified before deletion: `git bundle verify` reports "The bundle records a
complete history", and all 15 unpushed commits (`ca24865 a31e9d7 07ebb50
c63df34 6b2b135 f70168a 2837f64 21f7cbd fd9fbb4 a3eb5dc 8176f0e f31fbe7
35ac995 82a1178 e87f8e5`) are present in it.

Every file in that tree also exists in this repository, so the bundle is for
its history and its uncommitted notes, not for recovering source.

## `screenshot-rs-rebuild-uncommitted.diff`

The two uncommitted edits that were sitting in that tree — an `AGENTS.md` entry
for `docs/hyprland-findings.md` and `found.md` lesson 12 ("Cited upstream
default values from memory, which inverted a problem's root cause"), plus the
Hyprland findings verification note. All three were already folded into this
repository's `AGENTS.md` / `found.md` / `docs/hyprland-findings.md` (that doc is
byte-identical here), so the diff is a record, not a pending change.

## Not archived: the two `screenshot-plain` snapshots

`/home/life/Work/screenshot-plain` and `/home/work/Work/screenshot-plain` are
byte-identical 2026-10-24 snapshots whose `.git` files already pointed at a
nonexistent worktree (`/home/life/Work/MKSENSE/screenshot-rs-rebuild/.git/worktrees/screenshot-plain`)
before the old tree was removed, so neither could run any git command. Their
content is an older state of this repository's `plain` branch: `src/cli.rs`,
`selection.rs`, `screencopy.rs` and `geometry.rs` are identical to it, while
`app.rs`/`image.rs` predate the 0.2.2 save-path work. Nothing there exists only
there, so they are recoverable from `plain` and were not bundled.

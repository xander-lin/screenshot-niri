#!/usr/bin/env python3
"""Judge a DMS-harness run (placements.tsv) against the corpus's pairwise
pixel oracle (replay-frames's .truth-replay-oracle.cache).

Doctrine (same as replay-frames): a pair is *strict* when the oracle found an
unambiguous displacement with cost <= 1.0; a displacement outside the tie set
(+/-1 px) is refuted by the pixels. Pairs above that cost (scroll-transition
blends) or without overlap are not judged.

Per-frame outcome relative to the previous placed frame:
  SEAM        frame appended blind after 4 unmatched ticks (invisible seam);
              the anchor chain resets
  STALE       placed with anchor delta 0 while the pixels scrolled >=2
              (matched against stale canvas content: silent loss)
  CONTRA      placed, anchor delta outside the tie set (content misplaced)
  DUP-DROP    dropped as a duplicate while the pixels scrolled >=2
  REFUSE      pushFrame refused (a silent drop when the pixels scrolled >=2)
  ok          anchor delta inside the tie set

Usage:
    python3 analyze.py CORPUS_DIR PLACEMENTS.tsv [SEGMENT_DIR] [LABEL]

CORPUS_DIR is the corpus (session.json + frames + .truth-replay-oracle.cache,
produced by `replay-frames --dir CORPUS_DIR`). PLACEMENTS.tsv comes from a
`tools/dms-cmp` run. For constant-size segments of a geometry-edit corpus,
pass the segment directory so frame indices are mapped back to the corpus.
"""
import json
import os
import sys

MIN_I32 = -2147483648
STRICT_COST = 1.0   # replay-frames's blend-cost ceiling (ORACLE_BLEND_COST)
MOVED = 2           # |truth delta| below this counts as static


def load_manifest(corpus):
    p = os.path.join(corpus, "session.json")
    if os.path.exists(p):
        frames = json.load(open(p))["frames"]
        return [f["file"] for f in frames]
    return sorted(f for f in os.listdir(corpus) if f.endswith(".png"))


def load_oracle(corpus, names):
    lines = open(os.path.join(corpus, ".truth-replay-oracle.cache")).read().splitlines()
    assert len(lines) - 1 == len(names) - 1, "oracle cache length mismatch"
    out = []
    for ln in lines[1:]:
        d, cost, amb, ties = ln.split("\t")
        out.append((int(d), float(cost), amb == "1",
                    [int(t) for t in ties.split(",") if t]))
    return out


def analyse(corpus, tsv, run_names, label):
    names = load_manifest(corpus)
    idx = {n: i for i, n in enumerate(names)}
    oracle = load_oracle(corpus, names)
    rows = [ln.split("\t") for ln in open(tsv).read().splitlines()[1:]]
    assert len(rows) == len(run_names), (len(rows), len(run_names))

    counts = dict(seam=0, stale=0, contra=0, dupdrop=0, refuse=0, ok=0, unjudged=0)
    moved_pairs = 0
    strict_pairs = 0
    lost_rows = 0
    total_scroll = 0
    chain_breaks = 0
    chain_broken = False
    timings = []
    placed_n = 0
    placed_time = 0.0
    # (corpus idx, anchor, anchored): `anchored` says whether `anchor` is where
    # this frame's content actually sits. A placement or a seam re-anchors; a
    # duplicate keeps the previous anchor only while that was valid; a refused
    # frame invalidates it (the anchor still names the last placed frame, whose
    # content has scrolled away), so the next pair cannot be judged against a
    # single pair's truth. Note the harness logs seam rows with placed=false
    # (pushFrame failed before seamAppend ran), yet the anchor did advance.
    prev = None
    details = []
    for r, name in zip(rows, run_names):
        dup, placed, seam = r[1] == "true", r[2] == "true", r[3] == "true"
        anchor, ns = int(r[5]), int(r[7])
        timings.append(ns / 1e6)
        if placed:
            placed_n += 1
            placed_time += ns / 1e6
        ci = idx.get(name)
        if prev is None or ci is None or not prev[2]:
            counts["unjudged"] += 1
        elif ci == prev[0] + 1 and 0 <= ci - 1 < len(oracle):
            delta, cost, amb, ties = oracle[ci - 1]
            strict = delta != MIN_I32 and cost <= STRICT_COST
            if not strict:
                counts["unjudged"] += 1
            else:
                strict_pairs += 1
                moved = abs(delta) >= MOVED
                if moved:
                    moved_pairs += 1
                    total_scroll += abs(delta)
                if seam:
                    counts["seam"] += 1
                    if moved:
                        lost_rows += abs(delta)
                elif not placed:
                    kind = "DUP-DROP" if dup else "REFUSE"
                    counts["dupdrop" if dup else "refuse"] += 1
                    if moved:
                        lost_rows += abs(delta)
                        details.append(f"{name} {kind} truth={delta}")
                else:
                    d_dms = anchor - prev[1]
                    inside = any(abs(t - d_dms) <= 1 for t in ties)
                    if inside:
                        counts["ok"] += 1
                    elif d_dms == 0 and moved:
                        counts["stale"] += 1
                        lost_rows += abs(delta)
                        details.append(f"{name} STALE truth={delta}")
                    else:
                        counts["contra"] += 1
                        if moved:
                            lost_rows += abs(delta)
                        details.append(f"{name} CONTRA truth={delta} "
                                       f"ties={ties} dms={d_dms}")
        else:
            counts["unjudged"] += 1
        anchored = placed or seam or (dup and prev is not None and prev[2])
        if not anchored and prev is not None and prev[2]:
            chain_broken = True
        if (placed or seam) and chain_broken:
            # A placement that re-anchored after a refusal stretch: its anchor
            # difference spans several pairs and cannot be judged against one
            # pair's truth. Count it instead of guessing.
            chain_breaks += 1
            chain_broken = False
        prev = (ci, anchor, anchored)

    mean = sum(timings) / len(timings) if timings else 0.0
    acc = placed_time / placed_n if placed_n else 0.0
    cover = (1.0 - lost_rows / total_scroll) if total_scroll else float("nan")
    print(f"== {label}")
    print(f"   placed {placed_n}  ok {counts['ok']}  CONTRA {counts['contra']}"
          f"  STALE {counts['stale']}  DUP-DROP(moved) {counts['dupdrop']}"
          f"  REFUSE(moved) {counts['refuse']}  SEAM {counts['seam']}")
    print(f"   strict pairs {strict_pairs} (moved {moved_pairs}),"
          f" truth scroll {total_scroll} px, silently lost {lost_rows} px"
          f" ({0 if total_scroll == 0 else 100.0 * lost_rows / total_scroll:.1f}%),"
          f" coverage {100.0 * cover:.1f}%")
    print(f"   placements after a refusal stretch (not pair-judgeable):"
          f" {chain_breaks}")
    print(f"   per frame mean {mean:.3f} ms  worst {max(timings, default=0):.3f} ms"
          f"  per placed mean {acc:.3f} ms")
    for d in details[:12]:
        print(f"     {d}")
    if len(details) > 12:
        print(f"     ... {len(details) - 12} more")
    return counts


if __name__ == "__main__":
    corpus, tsv = sys.argv[1], sys.argv[2]
    if len(sys.argv) > 3 and sys.argv[3]:
        seg = sys.argv[3]
        run_names = sorted(os.path.basename(os.path.realpath(
            os.path.join(seg, f))) for f in os.listdir(seg) if f.endswith(".png"))
    else:
        run_names = load_manifest(corpus)
    analyse(corpus, tsv, run_names, sys.argv[4] if len(sys.argv) > 4 else corpus)

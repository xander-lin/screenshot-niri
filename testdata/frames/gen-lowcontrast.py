#!/usr/bin/env python3
"""Generate a low-contrast horizontal-stripe corpus for the stitcher.

The page is alternating runs of two very similar grays (one slightly darker,
one slightly lighter). Run lengths are random and bounded, so the content is
non-periodic and never a long flat block that could match anywhere.

Usage: gen-lowcontrast.py OUT_DIR [DELTA] [FRAMES]

DELTA is the luminance difference between the two grays (2 = very low).
"""
import json
import os
import random
import sys

from PIL import Image

FRAME_W, FRAME_H = 644, 254
PAGE_H = 8000
MIN_RUN, MAX_RUN = 12, 48
BASE = 128


def build_page(delta, rng):
    rows = []
    dark = True
    while len(rows) < PAGE_H:
        run = rng.randint(MIN_RUN, MAX_RUN)
        value = BASE if dark else BASE + delta
        rows.extend([value] * min(run, PAGE_H - len(rows)))
        dark = not dark
    return rows


def main():
    out = sys.argv[1]
    delta = int(sys.argv[2]) if len(sys.argv) > 2 else 2
    frame_count = int(sys.argv[3]) if len(sys.argv) > 3 else 120
    os.makedirs(out, exist_ok=True)
    rng = random.Random(20261020)
    page = build_page(delta, rng)

    positions = [0]
    # Jumpy wheel scrolling: still, then a 60 or 120 px tick.
    while len(positions) < frame_count:
        step = rng.choice([0, 0, 60, 60, 120])
        positions.append(min(positions[-1] + step, PAGE_H - FRAME_H))

    frames = []
    for index, top in enumerate(positions):
        img = Image.new("L", (FRAME_W, FRAME_H))
        for y in range(FRAME_H):
            img.paste(page[top + y], (0, y, FRAME_W, y + 1))
        name = f"frame-{index:04d}.png"
        img.convert("RGB").save(os.path.join(out, name))
        frames.append({"file": name, "width": FRAME_W, "height": FRAME_H, "direction": "down"})

    manifest = {
        "selection": {"x": 0, "y": 0, "width": FRAME_W, "height": FRAME_H},
        "region": {"output": 1, "x": 0, "y": 0, "width": FRAME_W, "height": FRAME_H},
        "fps": 30,
        "outputs": [{"name": 1, "x": 0, "y": 0, "width": FRAME_W, "height": FRAME_H, "scale": 1}],
        "frames": frames,
        "note": f"generated: alternating grays {BASE}/{BASE + delta}, random runs {MIN_RUN}-{MAX_RUN}px",
    }
    with open(os.path.join(out, "session.json"), "w") as f:
        json.dump(manifest, f, indent=1)
    print(f"{out}: {len(frames)} frames, delta={delta}, page={PAGE_H}, last={positions[-1]}")


if __name__ == "__main__":
    main()

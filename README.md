# screenshot

`screenshot` is a Wayland screenshot tool with region selection and long/scroll screenshot support, originally developed for **niri**.

- **Normal mode**: drag to select a region — the screen freezes and dims during selection.
- **Long mode**: capture scrolling content by selecting a region, then scrolling — the tool stitches frames into one tall image.

The binary is named `screenshot`.

## Compositor Compatibility

There is no compositor-name check: normal and long modes can be launched in any session. Compatibility is left to users to test in their environment.

Capture uses `wlr-screencopy` v3, the overlay uses `wlr-layer-shell`, and clipboard serving uses `wlr-data-control`. A Wayland connection and the required protocols must be available; missing capabilities are reported at runtime.

## Install

### From Source

```bash
cargo install --git https://github.com/xander-lin/screenshot-niri.git
```

## Usage

```
screenshot [OPTIONS] [PATH]
```

### Modes

| Flag | Mode | Description |
|------|------|-------------|
| _(default)_ | Normal | Drag to select a region, save, copy to clipboard |
| `--long` | Long/Scroll | Select a region, scroll to capture more, press Enter to finish |
| `--fps N` | — | Long-capture frame-rate cap in fps (default: 30, range: 1–240) |

### Output Path

The output path is resolved in this priority:

**1. Temporary (default)**
```bash
screenshot
# → $XDG_RUNTIME_DIR/screenshot-20250622-143052-123456789.png
#   or /tmp/screenshot-rust-{uid}/screenshot-20250622-143052-123456789.png
```
Saves to a temp location. The file is served to clipboard automatically.

**2. `--file` — Save to Pictures/Screenshots (with optional path)**
```bash
screenshot --file                      # → ~/Pictures/Screenshots/{timestamp}.png
screenshot --file /home/me/shots/      # → /home/me/shots/{timestamp}.png
screenshot --file /home/me/out.png     # → /home/me/out.png
screenshot --file --name capture       # → ~/Pictures/Screenshots/capture.png
```

**3. `-o` / `--output` — Exact output path**
```bash
screenshot -o /tmp/my-shot.png         # → /tmp/my-shot.png
screenshot --output /tmp/              # → /tmp/{timestamp}.png (trailing slash = dir)
```

**4. `--name` — Custom filename**
```bash
screenshot --name foo                  # → temp dir, named foo.png
screenshot --file --name bar           # → Pictures/Screenshots/bar.png
screenshot -o /tmp/ --name baz         # → /tmp/baz.png
```

**5. Positional PATH — same as `--file PATH`**
```bash
screenshot /home/me/shots/out.png      # → /home/me/shots/out.png
```

### Clipboard

| Flag | Clipboard content |
|------|-------------------|
| _(default)_ | `image/png` — paste directly into image apps, chat, etc. |
| `--url` | `file://` URI — paste as a file path |

### Configuration File

Optional `~/.config/screenshot/config` (or `$XDG_CONFIG_HOME/screenshot/config`), simple `key = value` lines, `#` comments. Invalid values fall back to defaults.

```ini
# Long-capture frame-rate cap (used when --fps is not given; 1-240)
max_fps = 30
# Visible selection-border thickness in logical pixels (1-16)
border_width = 3
```

### Normal Screenshot

1. Run `screenshot` — all screens freeze and dim.
2. **Drag** with the left mouse button to select a region.
3. **Release** — the screenshot saves and is copied to clipboard.
4. Press **Esc** at any time to cancel.
5. The frozen preview and the saved screenshot use the cursor-free frame by default. Press **H** to show the cursor (toggle between the captured with-cursor and without-cursor frames). The live selection cursor stays visible.
6. Press **1**–**9** to skip selection entirely and capture a full screen; screens are numbered left to right by their position.
7. Press **E** (during the drag or afterwards) to enter selection-box **editing mode**: each of the four box edges has a snap target you can grab and drag to resize the box. Press **E** again or **Enter/Space** to capture with the adjusted box.

### Long/Scroll Screenshot (`--long`)

1. Run `screenshot --long` — enter region selection.
2. **Drag** to select the capture area on a single output. If the box spans two outputs its border turns **red** as a warning; releasing there **discards the box** — as if nothing had been selected — so you can drag again on one output. Back on a single output the border turns white again.
3. The overlay dims except for the selected region. Capture is inset one logical pixel where the box is large enough. The closed border remains visible while selecting, resizing, and moving. Capture pauses during dragging; any unfinished frame exposed to a moved/resized border is discarded before stitching.
4. **Scroll** the content within the selected region.
   The selection border is always drawn above the stitched preview, with the same appearance before and after selection.
5. Press **↑** / **↓** / **←** / **→** to hint the scroll direction for better stitching. The hint can change mid-session: a **serpentine** path — right, down, left, down — stitches into one image, as long as consecutive frames still overlap. Each turn is matched against what is already on the canvas, so a wide page can be walked column by column.

   The hint's job is to name the direction, not to pin the axis: the stitcher follows the step it last verified and falls back to the other axis only when that step cannot be placed. A horizontal step needs a horizontal hint (←/→) — nothing in the frame says which side it moved to, so a sideways move under a vertical hint is refused rather than guessed.
6. Press **E** during capture to enter selection-box **editing mode**: the box edges can be dragged to resize it and the box interior can be dragged to move it; wheel events are blocked (the displayed content cannot scroll while editing). Resizing is stitched at the exact edited position, which enables:
   - capturing content that is narrower on top and wider on the bottom (T-shaped captures),
   - scrolling vertically first and horizontally afterwards (L-shaped captures, hint with **←** / **→**),
   - extending the bottom edge after scrolling so static content (headers/footers that repeat while scrolling and would break stitching) is captured once, perfectly attached below.
   You can change width repeatedly (wide → narrow → wide). Previously captured pixels are preserved. Rows captured only at the narrower width retain transparent side notches unless you revisit and capture those missing areas; widening fills newly captured strips in the current viewport.
7. **Sticky page chrome is not compensated.** A pinned nav bar, frozen table header, or fixed footer inside the box does not scroll with the page, so it cannot be matched consistently. The stitcher does not detect or crop it. Use **E** to shrink the box past these strips before scrolling — a selection that includes them is a selection error, not a stitching failure.
8. Press **Enter** or **Space** to finish — the tool stitches all frames and saves.
9. Press **Esc** to cancel.
10. Long screenshots always use cursor-free capture frames; **H** applies only to normal screenshots. Long capture is capped at 30 fps by default (`--fps` or the config file's `max_fps` to change); frames are damage-driven, so slower machines and static content naturally capture less often.
11. **If the border turns red, the stitcher refused the current frame.** This happens when content jumped too far to be matched (a fast fling, Page Down, a tab switch): rather than guessing and producing a silently misaligned image, it keeps the last verified position and tells you. Scroll back until the red content overlaps what you already captured and the border returns to white by itself — nothing is lost. While the border is red, capture drops to 5 fps to save CPU; it returns to full rate immediately after a successful placement. A single frame that cannot be placed does not turn the border red: the stitcher only reports a failure after eight refusals in a row (about a quarter of a second), so an isolated ambiguous frame never makes the border flicker.

> **Tip**: Scroll slowly for best results. The stitcher tracks frame overlap automatically; direction hints improve accuracy with fast or jumpy scrolling.

### Examples

```bash
# Quick screenshot, paste anywhere
screenshot

# Save to screenshots folder with timestamp
screenshot --file

# Long screenshot of a web page or terminal
screenshot --long --file --name scroll-capture

# Specific path, custom name
screenshot -o ~/shots/bug-report.png

# Paste as file path instead of image
screenshot --url --file --name receipt
```

## Build from Source

```bash
git clone https://github.com/xander-lin/screenshot-niri.git
cd screenshot-niri
cargo build --release
# binary: target/release/screenshot
```

Requires Rust 1.80+.

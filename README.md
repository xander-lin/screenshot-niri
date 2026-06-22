# screenshot-plain

**screenshot-plain** is a lightweight Wayland screenshot tool for wlroots compositors.  
Select a region by dragging — the screen freezes and dims during selection, so you see exactly what will be captured.

This is the **plain** variant — normal screenshots only, no long/scroll capture, no compositor lock-in.  
For the full variant with scroll screenshot support, see the `master` branch.

## Supported Compositors

Uses standard wlroots protocols. Works on any compositor that exposes them:

| Compositor |               |
|-----------|---------------|
| Niri      | ✅ |
| Hyprland  | ✅ |
| Sway      | ✅ |
| Wayfire   | ✅ |
| River     | ✅ |

Required protocols: `zwlr_screencopy_manager_v1`, `zwlr_layer_shell_v1`, `wlr-data-control`.

## Install

### From Source

```bash
cargo install --git https://github.com/xander-lin/screenshot-niri.git --branch plain
```

The binary is named `screenshot` by the build. Rename to `screenshot-plain` if desired.

### Arch Linux

```bash
paru -S screenshot-plain
```

## Usage

```
screenshot [OPTIONS] [PATH]
```

### Quick Start

| Command | Behavior |
|---------|----------|
| `screenshot` | Select a region → save to temp dir, copy image to clipboard |
| `screenshot --file` | Same, but save to `~/Pictures/Screenshots/` |
| `screenshot --url` | Copy file URI to clipboard instead of image data |

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

### Selection

1. Run the command — all screens freeze and dim.
2. **Drag** with the left mouse button to select a region.
3. **Release** — the screenshot saves and is copied to clipboard.
4. Press **Esc** at any time to cancel.

### Examples

```bash
# Quick screenshot, paste anywhere
screenshot

# Save to screenshots folder with timestamp
screenshot --file

# Specific path, custom name
screenshot -o ~/shots/bug-report.png

# Paste as file path instead of image
screenshot --url --file --name receipt

# One-liner to a fixed location
screenshot ~/Pictures/screenshot.png
```

## Build from Source

```bash
git clone https://github.com/xander-lin/screenshot-niri.git
cd screenshot-niri
git checkout plain
cargo build --release
# binary: target/release/screenshot
```

Requires Rust 1.80+.

## Project Size

~2,600 lines of Rust. No unsafe beyond `libc::mmap` for Wayland shared memory and `libc::localtime_r` for timestamp filenames.

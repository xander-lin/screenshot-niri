# screenshot

`screenshot` is a Wayland screenshot tool for **niri** with region selection and long/scroll screenshot support.

- **Normal mode**: drag to select a region — the screen freezes and dims during selection.
- **Long mode**: capture scrolling content by selecting a region, then scrolling — the tool stitches frames into one tall image.

The binary is named `screenshot`.

## Supported Compositor

| Compositor |               |
|-----------|---------------|
| Niri      | ✅ |
| Hyprland  | ❌ (use the `plain` branch) |
| Sway      | ❌ (use the `plain` branch) |

Uses niri-specific wlroots protocol extensions. The `plain` branch supports all wlroots compositors but without long screenshot mode.

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

### Normal Screenshot

1. Run `screenshot` — all screens freeze and dim.
2. **Drag** with the left mouse button to select a region.
3. **Release** — the screenshot saves and is copied to clipboard.
4. Press **Esc** at any time to cancel.

### Long/Scroll Screenshot (`--long`)

1. Run `screenshot --long` — enter region selection.
2. **Drag** to select the capture area on a single output.
3. The overlay dims except for the selected region. The tool starts capturing frames.
4. **Scroll** the content within the selected region.
5. Press **↑** / **↓** to hint the scroll direction for better stitching.
6. Press **Enter** or **Space** to finish — the tool stitches all frames and saves.
7. Press **Esc** to cancel.

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

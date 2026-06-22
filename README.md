# screenshot-plain

**screenshot-plain** is a lightweight Wayland screenshot tool for wlroots compositors.  
It supports interactive region-based screenshots via drag selection with a frozen-background overlay.

This is the **plain** variant — no long screenshots, no stitching, no compositor lock-in.  
For the full variant with long/scroll screenshot support, see the `master` branch.

## Supported Compositors

Uses standard wlroots protocols only. Works on any compositor that exposes them:

| Compositor | Status |
|-----------|--------|
| Niri      | ✅ |
| Hyprland  | ✅ |
| Sway      | ✅ |
| Wayfire   | ✅ |
| River     | ✅ |

Required protocols:
- `zwlr_screencopy_manager_v1` — output capture
- `zwlr_layer_shell_v1` — selection overlay
- `wlr-data-control` — clipboard

## Install

### From Source

```bash
cargo install --git https://github.com/xander-lin/screenshot-niri.git --branch plain
# installs as `screenshot` — rename to screenshot-plain if desired
```

### Arch Linux (AUR)

```bash
paru -S screenshot-plain
```

## Usage

```
screenshot-plain [OPTIONS] [PATH]
```

| Flag | Description |
|------|-------------|
| `-h`, `--help` | Show help |
| `--file [PATH]` | Save to PATH, or Pictures/Screenshots/ |
| `-o`, `--output PATH` | Save to PATH |
| `--name NAME` | Set output filename (.png appended) |
| `--url` | Copy file URI instead of image/png |

### Workflow

1. Run `screenshot-plain` — the screen freezes and dims
2. Drag to select a region
3. Release — the screenshot saves and copies to clipboard
4. Press `Esc` to cancel

## Build

```bash
git clone https://github.com/xander-lin/screenshot-niri.git
cd screenshot-niri
git checkout plain
cargo build --release
# binary at target/release/screenshot
```

Requires Rust 1.80+.

## Project Size

~2,600 lines of Rust. No unsafe beyond `libc::mmap` for shared memory buffers and `libc::localtime_r` for filename generation.

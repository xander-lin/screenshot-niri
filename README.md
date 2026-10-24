# screenshot

`screenshot` is a Wayland screenshot tool with region and full-screen capture,
originally developed for **niri**. The screen freezes and dims during selection,
so the capture is exactly what you saw.

This is the **plain** variant: normal screenshots only. Long/scroll capture,
stitching, and the corpus tooling live on the `niri` branch and are not part of
this build.

The binary is named `screenshot`.

## Compositor Compatibility

There is no compositor-name check: the tool can be launched in any session.
Compatibility is left to users to test in their environment.

Capture uses `wlr-screencopy` v3, the overlay uses `wlr-layer-shell`, and
clipboard serving uses `wlr-data-control`. A Wayland connection and the required
protocols must be available; missing capabilities are reported at runtime.

## Install

### From Source

```bash
cargo install --git https://github.com/xander-lin/screenshot-niri.git --branch plain
```

### Prebuilt binaries

The x86_64 release tarballs are built with `-C target-cpu=x86-64-v3`
(AVX2/BMI/FMA: Intel Haswell 2013 / AMD Excavator 2015 or later). A
`-x86_64-generic` asset is published for older CPUs; aarch64 uses the
architecture default. The AUR package checks the local CPU while building and
installs the v3 or generic binary automatically — exactly one binary, no
runtime launcher and no manual choice.

### Arch Linux

```bash
paru -S screenshot-plain
```

The package conflicts with `screenshot-niri` because both install
`/usr/bin/screenshot`.

## Usage

```
screenshot [OPTIONS] [PATH]
```

### Interaction

| Key / action | Behaviour |
|---|---|
| Drag | Select a region; release saves and copies to the clipboard |
| `1`–`9` | Capture a full screen; screens are numbered left to right (before a drag starts) |
| `0` | Capture every screen as one image (before a drag starts) |
| `H` | Toggle the cursor in the capture; press while selecting, before release (hidden by default) |
| `Esc` | Cancel at any time |

The digit keys and `0` work before the first mouse press; once a drag has
started the box is what is being selected. Releasing the mouse finishes the
capture. A selection spanning several outputs is supported: each output's piece
is captured and they are laid out end to end, so a gap between outputs is
closed rather than painted black.

### Output Path

The output path is resolved in this priority:

**1. Temporary (default)**
```bash
screenshot
# → $XDG_RUNTIME_DIR/screenshot-20250622-143052-123456789.png
#   or /tmp/screenshot-rust-{uid}/screenshot-20250622-143052-123456789.png
```
Saves to a temp location. The file is served to the clipboard automatically.

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

The clipboard is served by a detached `--clipboard-provider` process (Wayland
requires the data source to stay alive). That lingering process is expected.

### Configuration

Optional file at `$XDG_CONFIG_HOME/screenshot/config` (default
`~/.config/screenshot/config`). Simple `key = value` lines; `#` starts a
comment. Unknown keys and invalid values fall back to defaults, so a
hand-edited file can never break the tool.

```ini
# selection-border thickness in logical pixels (1-16, default 3)
border_width = 3
```

## Examples

```bash
# Quick screenshot, paste anywhere
screenshot

# Save to screenshots folder with timestamp
screenshot --file

# Specific path, custom name
screenshot -o ~/shots/bug-report.png

# Paste as a file path instead of image data
screenshot --url --file --name receipt
```

## Development

```bash
cargo build --release        # binary: target/release/screenshot
cargo test                   # unit tests
cargo clippy --all-targets   # warnings are treated as review material
```

Trace logs: build with `--features trace-logs` to print overlay/capture
diagnostics to stderr (`trace_log!` is a no-op without the feature).

Packaging: `PKGBUILD` builds package `screenshot-plain`, which conflicts with
`screenshot-niri`.

## License

MIT — see `LICENSE`.

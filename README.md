# screenshot-rs-rebuild

`screenshot-rs-rebuild` is a fresh, niri-only Rust screenshot tool. The binary is named `screenshot`.

## Scope

- Targets niri on Wayland through wlroots protocols exposed by niri.
- Supports normal interactive region screenshots only.
- Does not include Hyprland, Sway, GNOME, KDE, X11, long screenshot, stitching, focus-grab, or diagnostics compatibility paths.
- Performs a lightweight niri-session preflight before normal capture using `NIRI_SOCKET` or `XDG_CURRENT_DESKTOP` containing `niri`.
- Rejects `--long` with a clear unsupported-mode error.

## Runtime Flow

1. Parse CLI output and clipboard options.
2. Reject normal capture outside a niri session with a niri-only diagnostic.
3. Capture and freeze all outputs with `zwlr_screencopy_manager_v1`.
4. Run a layer-shell overlay for drag-based region selection.
5. Composite selected output regions into one image.
6. Write PNG to the selected path.
7. Serve the written file to the clipboard through `wlr-data-control` as either `image/png` or file URI data.

## Module Map

- `cli`: argument parsing, help text, and output-path resolution.
- `app`: top-level normal screenshot orchestration, niri preflight, and long-mode rejection.
- `runtime`: lightweight niri-session environment detection.
- `geometry`: logical-output and selected-region math.
- `image`: captured-image representation, compositing, and PNG writing.
- `clipboard`: detached clipboard provider and `wlr-data-control` source handling.
- `wayland::screencopy`: wlroots screencopy capture and frozen-output collection.
- `wayland::selection`: niri/wlr layer-shell overlay selection.

## Verification

Developer implementation did not run commands. Expected validation is `cargo test` for pure logic followed by `cargo check`, then a manual niri session test for selection, capture, file writing, and clipboard modes.

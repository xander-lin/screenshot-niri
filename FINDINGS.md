# Durable Findings

## Project Scope

- `screenshot-rs-rebuild` is a wlroots-based Wayland screenshot tool with binary name `screenshot`. This is the `plain` branch.
- Supported runtime targets: any wlroots compositor (Niri, Hyprland, Sway, Wayfire, River, etc.).
- Normal interactive region screenshots are in scope.
- Out of scope: X11, long screenshots, stitching, focus-grab behavior, and non-wlroots compositors.

## Module Relationships

- `cli` parses arguments and resolves output paths for `app`.
- `app` orchestrates the normal screenshot flow: frozen screencopy capture, selection, compositing, file write, and clipboard startup.
- `runtime` provides optional niri-session detection utilities (kept for diagnostics, not used as a gate).
- `wayland::screencopy` captures and freezes output frames for later region selection and compositing.
- `wayland::selection` draws the frozen captured frames in the layer-shell overlay while collecting drag-selected geometry.
- `wayland::selection` caches per-overlay base and dimmed frozen buffers after layer-surface configure so pointer-motion redraws avoid per-pixel frozen resampling and alpha blending.
- `geometry` maps selected logical regions to per-output capture regions.
- `image` represents captured buffers, composites selected regions, and writes PNG files.
- `clipboard` serves the saved screenshot through `wlr-data-control` as `image/png` or file URI data.
- The `stitch` module has been removed from this branch — long screenshots are out of scope.

## Architecture Decisions

- The `plain` branch removes all long-screenshot functionality, keeping only normal interactive screenshots.
- The niri-only session check has been removed — the tool targets all wlroots compositors.
- No compositor-specific preflight or adaptation branches exist.

## Verification Memory

- Automated tests: `cargo test` passes with all 29 tests (cli, geometry, image, runtime, clipboard, selection).
- Manual niri session testing before the frozen-background change was reported successful for selection, capture, file writing, and clipboard behavior.

## Maintenance Rule

- Current code, project documentation, tests, and command results remain authoritative. This file is only durable memory for recurring project context and should be updated only with stable, useful findings.

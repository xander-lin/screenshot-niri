# Overlay resolution and fractional scaling

Status: merged into `niri` (`c16711f`); hardware verification pending.

## Two symptoms, one root cause

1. On a 1.5-scaled output the frozen selection overlay was soft while the saved
   PNG was sharp.
2. In normal mode, pressing `1` (capture that output) produced no file at all,
   and any selection spanning it produced no file either; the unscaled output
   worked.

Both assumed that `wl_output.scale` converts logical coordinates to output
pixels. It does not: it is an **integer hint** (1.5 is not representable, so
compositors report 1 or 2, and the protocol does not say which), and the true
pixel grid is only visible in the screencopy buffer size.

### Why the overlay was soft

The overlay surface is created with a logical-sized buffer and buffer scale 1.
On a 1920x1080 mode at scale 1.5 the logical size is 1280x720, so the frozen
1920x1080 capture was point-sampled down to 1280x720 and the compositor scaled
it back up:

```
1920x1080 capture -> nearest neighbour -> 1280x720 buffer -> compositor x1.5 -> 1920x1080
```

The saved PNG never takes that path, which is why it stayed sharp.

### Why the capture failed

`SelectedViewport::capture_regions` multiplied the logical rectangle by
`output.scale`. With the compositor reporting 2 for a 1.5 output, a full-screen
selection on that output asked for 2560x1440 pixels from a 1920x1080 image;
`blit_image_region` rejected it and the whole composite (and therefore the PNG
and the clipboard) was abandoned. A single failing piece kills the image, so any
spanning selection that touched the scaled output failed too.

## Design

### Overlay buffer size

```
buffer = max(content pixels, ceil(logical x preferred_scale / 120))
```

- `content` is the frozen capture when there is one, and the output's current
  `wl_output.mode` otherwise: a `--long` session starts live, so before the
  first capture the mode is the only known pixel grid (and it is the grid the
  live capture will arrive at). The capture wins over the mode because it is
  what we display.
- `preferred_scale` comes from `wp_fractional_scale_v1` (180 = 1.5); niri does
  not advertise that protocol, so there the mode is what makes a 1.5-scaled
  long-capture overlay sharp.
- Two hard lower bounds: never below the content that is about to be displayed
  (otherwise we resample our own screenshot before the compositor shows it) and
  never below the output's pixel density (otherwise the compositor upscales the
  overlay).
- Without `wp_viewporter` a buffer larger than the surface cannot be mapped
  back, so the logical size wins and the old blur returns.
- `buffer_scale` stays 1; a `wp_viewport` destination equal to the logical size
  maps the denser buffer onto the surface.

`refresh_overlay_surface` owns this. It runs from the layer-surface configure
(logical size) and from `preferred_scale` (density); either can arrive first and
either can change independently.

### Renderers

Everything downstream works in **buffer pixels** and converts once:

- frozen path: `draw_overlay_region` maps each buffer pixel to global logical
  coordinates before testing the hole and the closed border ring; the dirty ring
  uses `buffer_length` to express the configured logical width in buffer pixels;
- live path: the hole and the dirty region are already buffer-mapped
  (`selected_buffer_rect`), the border thickness is converted with
  `buffer_length`, and the stitched preview is drawn **buffer-major**: each
  buffer pixel of the dimmed area picks the stitched pixel it covers. Iterating
  the stitched image instead would leave gaps as soon as the buffer is denser
  than the logical grid, and the live screen would show through them.
- `frozen_pixel` is a pure ratio (`buffer -> capture`); the old
  `wl_output.scale` shortcut is gone, since it is wrong whenever the buffer is
  the capture size on an integer-scaled output.

The live path keeps the larger buffer for the whole session instead of resizing
on the `L` handover: no allocation burst mid-session, and the live overlay
(border, preview) is rendered at the output's density.

### Normal-mode composite

`SelectedViewport::capture_regions` takes the captures and derives each piece's
pixel ratio from the image itself (`image.width / output logical width`).
Edges, not widths, are mapped, so adjacent pieces share one boundary even when
the ratio is fractional:

```
x      = floor(local.x * image.width / logical.width)
width  = floor(local.right() * ...) - x
```

The division truncates rather than rounds, and both edges go through it, so a
piece's width is exactly the distance between the pixels its edges map to —
rounding each width independently would leave a one-pixel seam.

Destination offsets in a mixed-scale spanning selection use the same ratios, so
pieces stay contiguous.

## Cost

Overlay buffers and both frozen caches grow by `scale^2` (2.25x at 1.5). On a
1920x1080 mode that is roughly 22 MB -> 50 MB per output.

## Tests

- `overlay_buffer_size_never_shrinks_below_content_or_density`
- `frozen_overlay_maps_buffer_pixels_through_the_logical_grid` (4x4 logical,
  6x6 buffer): reverting the mapping to 1:1 fails it.
- `preview_fills_a_denser_buffer_without_gaps` (2x2 logical, 4x4 buffer): the
  forward-iteration implementation leaves gaps and fails it.
- `mixed_scale_selection_places_pieces_contiguously`: a spanning selection that
  starts inside a fractionally scaled output. Mapping the destination offset in
  global coordinates instead of the output's local frame adds the phase of
  `origin * ratio` and places the next piece one column early (142 instead of
  143) — the seam overlaps by one pixel.
- `capture_regions_follow_the_capture_pixel_ratio_on_fractional_scale`:
  reverting the ratio to `output.scale` asks for 200x160 from a 150x120 capture
  and fails it — the exact normal-mode failure.

## Hardware checklist

1. Scale 1.5: the frozen selection image must match the pre-freeze desktop
   pixel for pixel; the 1 px border must be sharp.
2. Scale 1.5, press `1`: a full 1920x1080 PNG.
3. Scale 1.5 + 1.0 spanning selection: one file, pieces aligned at the seam.
4. Scale 1.0: unchanged behaviour (buffer == logical, same code path).
5. Long mode: preview alignment in the hole, edit-mode border tracking, `L`
   handover without a resize stall.

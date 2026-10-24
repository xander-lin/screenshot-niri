package main

// Equivalent of the two package-level helpers stitch.go expects from its
// original package (region_render.go / scroll.go). Logic copied verbatim
// from DMS v1.6.3; only the seam-tick constant is needed here.

const scrollSeamTicks = 4

func clamp(v, lo, hi int) int {
	if v < lo {
		return lo
	}
	if v > hi {
		return hi
	}
	return v
}

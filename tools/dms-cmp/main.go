package main

// Harness: feed the real-1 corpus (real screencopy frames) through the DMS
// v1.6.3 stitcher plus the scroll.go capture state machine (duplicate
// handling, unmatched ticks, seamAppend), logic unchanged. Records per-frame
// wall time of the stitcher work (rowSamples + frameSig + duplicateFrame +
// pushFrame/seamAppend — PNG decode excluded) and dumps placements + the
// final canvas for offline correctness analysis.
import (
	"fmt"
	"image"
	"image/png"
	"os"
	"path/filepath"
	"sort"
	"time"
)

func main() {
	dir := os.Getenv("DIR")
	if dir == "" {
		dir = "/home/life/Work/MKSENSE/screenshot-rs-rebuild/testdata/frames/real-1"
	}
	entries, err := os.ReadDir(dir)
	if err != nil {
		panic(err)
	}
	var files []string
	for _, e := range entries {
		if n := e.Name(); filepath.Ext(n) == ".png" {
			files = append(files, filepath.Join(dir, n))
		}
	}
	sort.Strings(files)
	if len(files) < 2 {
		panic("need frames")
	}

	// Decode all frames up front (outside the timed section).
	frames := make([][]byte, 0, len(files))
	var w, h int
	for _, f := range files {
		fh, err := os.Open(f)
		if err != nil {
			panic(err)
		}
		img, err := png.Decode(fh)
		fh.Close()
		if err != nil {
			panic(err)
		}
		b := img.Bounds()
		if w == 0 {
			w, h = b.Dx(), b.Dy()
		}
		rgba := image.NewRGBA(image.Rect(0, 0, w, h))
		for y := 0; y < h; y++ {
			for x := 0; x < w; x++ {
				rgba.Set(x, y, img.At(b.Min.X+x, b.Min.Y+y))
			}
		}
		frames = append(frames, rgba.Pix)
	}
	fmt.Printf("frames %d (%dx%d)\n", len(frames), w, h)

	st := newStitcher(w * 4)
	var prevSig []float32
	var prevPlaced, unmatched bool
	var unmatchedTicks int

	type row struct {
		idx                      int
		dup, placed, seam        bool
		added, anchor, canvas    int
		ns                       time.Duration
	}
	var log []row
	var seamCount, dupCount, refused int

	for i, px := range frames {
		t0 := time.Now()
		cols := st.rowSamples(px)
		sig := st.frameSig(px)
		dup := duplicateFrame(sig, prevSig)
		prevSig = sig

		var added int
		placed := false
		seam := false
		switch {
		case dup && unmatched:
			unmatchedTicks++
			if unmatchedTicks < scrollSeamTicks {
				break
			}
			added, placed = st.pushFrame(px, cols)
			if !placed {
				added = st.seamAppend(px, cols)
				seam = true
			}
			prevPlaced = true
			unmatched = false
			unmatchedTicks = 0
		case dup && prevPlaced:
			// settled duplicate: nothing to do
		default:
			added, placed = st.pushFrame(px, cols)
			prevPlaced = placed
			unmatched = !placed
			unmatchedTicks = 0
		}
		elapsed := time.Since(t0)
		if dup {
			dupCount++
		}
		if seam {
			seamCount++
		}
		if !placed && !dup {
			refused++
		}
		log = append(log, row{i, dup, placed, seam, added, st.anchor, st.rows(), elapsed})
	}

	// Aggregate.
	var total time.Duration
	var maxT time.Duration
	var placedTotal time.Duration
	placedN, placedAdded := 0, 0
	for _, r := range log {
		total += r.ns
		if r.ns > maxT {
			maxT = r.ns
		}
		if r.placed {
			placedTotal += r.ns
			placedN++
			placedAdded += r.added
		}
	}
	sortDur := func(d []time.Duration) time.Duration {
		sort.Slice(d, func(i, j int) bool { return d[i] < d[j] })
		return d[len(d)/2]
	}
	var all, acc []time.Duration
	for _, r := range log {
		all = append(all, r.ns)
		if r.placed {
			acc = append(acc, r.ns)
		}
	}
	fmt.Printf("placed %d  dup %d  seam %d  refused %d\n", placedN, dupCount, seamCount, refused)
	fmt.Printf("canvas rows %d  added rows %d\n", st.rows(), placedAdded)
	fmt.Printf("per frame : mean %.3f ms  median %.3f ms  worst %.3f ms  total %.1f ms\n",
		float64(total.Microseconds())/float64(len(log))/1e3,
		float64(sortDur(all).Microseconds())/1e3,
		float64(maxT.Microseconds())/1e3,
		float64(total.Microseconds())/1e3)
	fmt.Printf("per placed: mean %.3f ms  median %.3f ms\n",
		float64(placedTotal.Microseconds())/float64(placedN)/1e3,
		float64(sortDur(acc).Microseconds())/1e3)

	// Placements dump.
	pf, err := os.Create("placements.tsv")
	if err != nil {
		panic(err)
	}
	defer pf.Close()
	fmt.Fprintln(pf, "idx\tdup\tplaced\tseam\tadded\tanchor\tcanvas_rows\telapsed_ns")
	for _, r := range log {
		fmt.Fprintf(pf, "%d\t%v\t%v\t%v\t%d\t%d\t%d\t%d\n",
			r.idx, r.dup, r.placed, r.seam, r.added, r.anchor, r.canvas, r.ns.Nanoseconds())
	}

	// Canvas dump: st.canvas holds len(cols) rows of RGBA at stride w*4.
	out, err := os.Create("dms_canvas.png")
	if err != nil {
		panic(err)
	}
	defer out.Close()
	img := &image.RGBA{Pix: st.canvas, Stride: w * 4, Rect: image.Rect(0, 0, w, st.rows())}
	if err := png.Encode(out, img); err != nil {
		panic(err)
	}
	fmt.Println("canvas written to dms_canvas.png")
}

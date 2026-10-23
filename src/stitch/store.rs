//! Tiled storage for the stitched pixel canvas, compressed on demand.
//!
//! A long capture keeps every pixel it has ever stitched, and it keeps them for
//! as long as the session runs: the canvas *is* the output, so nothing can be
//! dropped. Measured on the real corpora, that is 16.5 MB for an 8 534-row
//! capture and 34.9 MB for 12 120 rows — and the whole thing is held as one
//! contiguous `Vec<u8>` that is reallocated and re-copied on every accepted
//! frame.
//!
//! Most of it is never read again. The stitcher touches the previous frame's
//! rectangle (a few hundred rows) and the overlay displays at most one screenful
//! mapped into the canvas; everything above that — every row the user has
//! already scrolled past — is only ever read again by the final PNG encode.
//!
//! So the canvas is stored as row bands (tiles). A tile is written
//! uncompressed, and once it has gone cold it is packed; reading it again
//! unpacks it into the working set. A byte budget bounds how much stays
//! unpacked, evicted least-recently-used first, so the resident footprint is
//! the budget plus whatever the capture is actively touching, instead of the
//! whole canvas.
//!
//! Measured on the real canvases at 64-row tiles (lz4, the default codec):
//! 5.5% of raw on `real-1` (16.5 MB → 0.91 MB) and 9.1% on `real-long`
//! (34.9 MB → 3.2 MB), against zstd-1's 4.0% and 6.3%. zstd buys about a
//! quarter more ratio for a C toolchain in the build; packing happens once per
//! tile life while unpacking happens on every access, and at these tile sizes
//! the two codecs decode at 0.59 and 0.40 µs/KB — both far below the frame
//! work they sit behind.
//!
//! Nothing here is required for correctness: the `raw` codec keeps every tile
//! resident, which is the pre-existing behaviour, and is what a budget of 0
//! selects.

use std::cell::{Cell, RefCell};

use super::types::CANVAS_TILE_ROWS;

/// One row band of the canvas, in one of three states.
///
/// `Empty` allocates nothing: a ragged canvas (the leg of an L, the space
/// between the legs of an arch) has never-captured regions, and they cost zero
/// bytes until something is written into them. The contiguous `Vec<u8>` this
/// replaces allocated the whole bounding box up front and zeroed it.
enum Tile {
    Empty,
    /// Uncompressed, and in the working set. `len()` is a whole number of rows.
    Resident { data: Vec<u8>, used: u64 },
    /// Packed; `len()` is a whole number of rows when unpacked.
    Packed { data: Vec<u8>, len: usize },
}

/// How a cold tile is packed. Two implementations ship; which one is built in
/// is a build-time choice (`raw` keeps everything resident, `lz4` packs).
pub(super) trait TileCompressor {
    fn name(&self) -> &'static str;
    /// Packs `src`. May return something no larger than `src`; the store treats
    /// the result as opaque.
    fn pack(&self, src: &[u8]) -> Vec<u8>;
    /// Unpacks `src` into `dst`, which is exactly the original length. Returns
    /// false if the payload does not describe `dst` (corruption, a length
    /// mismatch): the store then keeps the tile packed and reports zeros rather
    /// than panicking.
    fn unpack(&self, src: &[u8], dst: &mut [u8]) -> bool;
    /// Whether `pack` can actually shrink a tile. When it cannot, the store
    /// never pays a copy to find out.
    fn packs(&self) -> bool {
        true
    }
}

/// No compression: every touched tile stays resident for the session.
#[cfg(feature = "canvas-raw-tiles")]
pub(super) struct RawTiles;

#[cfg(feature = "canvas-raw-tiles")]
impl TileCompressor for RawTiles {
    fn name(&self) -> &'static str {
        "raw"
    }

    fn pack(&self, src: &[u8]) -> Vec<u8> {
        src.to_vec()
    }

    fn unpack(&self, src: &[u8], dst: &mut [u8]) -> bool {
        if src.len() != dst.len() {
            return false;
        }
        dst.copy_from_slice(src);
        true
    }

    fn packs(&self) -> bool {
        false
    }
}

/// LZ4 block compression, one independent block per tile.
///
/// Independent blocks are the point: a tile must be readable without touching
/// its neighbours, or reading one row would drag the whole canvas back into
/// memory. The cost is the ratio — a frame of context is lost at every band
/// boundary — and it is measured above.
pub(super) struct Lz4Tiles;

impl TileCompressor for Lz4Tiles {
    fn name(&self) -> &'static str {
        "lz4"
    }

    fn pack(&self, src: &[u8]) -> Vec<u8> {
        lz4_flex::block::compress(src)
    }

    fn unpack(&self, src: &[u8], dst: &mut [u8]) -> bool {
        match lz4_flex::block::decompress_into(src, dst) {
            Ok(written) => written == dst.len(),
            Err(_) => false,
        }
    }
}

/// The codec this build uses. Feature-gated the same way `new_placement_engine`
/// selects a matcher: one `cfg` decides, and the production build carries only
/// the codec it ships.
pub(super) fn new_tile_compressor() -> Box<dyn TileCompressor> {
    #[cfg(feature = "canvas-raw-tiles")]
    {
        Box::new(RawTiles)
    }
    #[cfg(not(feature = "canvas-raw-tiles"))]
    {
        Box::new(Lz4Tiles)
    }
}

/// What the store currently costs, for the memory report and the tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CanvasMemoryReport {
    /// Bytes the canvas covers if every row were resident: `height * stride`.
    pub logical_bytes: usize,
    /// Bytes the resident tiles actually occupy.
    pub resident_bytes: usize,
    /// Bytes the packed tiles occupy.
    pub packed_bytes: usize,
    /// Tiles that have never been written.
    pub empty_tiles: usize,
    pub resident_tiles: usize,
    pub packed_tiles: usize,
    pub codec: &'static str,
}

/// The stitched canvas's pixels, in row bands, packed when cold.
pub(crate) struct CanvasStore {
    width: u32,
    stride: u32,
    /// Canvas row of the first row of `tiles[0]`.
    ///
    /// The tile grid is anchored here rather than at canvas row 0 because
    /// growth happens in both directions: a frame that extends the canvas
    /// upward prepends rows, and re-cutting every band to keep the grid aligned
    /// with row 0 would copy the whole canvas. Moving the anchor by whole tiles
    /// moves band boundaries instead — `Vec::insert` on a `Vec<Tile>`, which
    /// moves 40-byte headers, not pixels. The cost is that up to one band of
    /// rows above the canvas top exists and is never read.
    origin_row: i32,
    tiles: RefCell<Vec<Tile>>,
    /// One flag per row of the grid, indexed by `row - origin_row`.
    ///
    /// A tile is allocated whole, so it holds rows that were never written: the
    /// tail of the band a frame ended in, and every row of a band a ragged
    /// canvas only partly reached. Those must read as *never captured*, exactly
    /// as they did when the canvas was a buffer with one row per byte range and
    /// anything past its end was out of bounds. Reading them as zeros instead
    /// made the colour check compare a frame against black. Cheap enough to keep
    /// honest: one byte per row, 8.5 KB on the longest measured canvas.
    written: RefCell<Vec<bool>>,
    compressor: Box<dyn TileCompressor>,
    /// Bytes of resident tiles allowed before cold ones are packed. 0 disables
    /// packing entirely (every touched tile stays resident).
    budget: usize,
    resident_bytes: Cell<usize>,
    packed_bytes: Cell<usize>,
    clock: Cell<u64>,
    /// Tiles touched since the last eviction sweep.
    ///
    /// Eviction sorts the resident set, so running it on every row read would
    /// cost more than the unpacking it saves. A sweep every `EVICT_INTERVAL`
    /// touches, taking the working set back to `budget * 3 / 4`, amortises the
    /// sort and leaves headroom so the next sweep is not immediate.
    touches_since_sweep: Cell<u32>,
}

impl CanvasStore {
    /// Resident bytes allowed before cold tiles are packed.
    ///
    /// Chosen against what a frame actually reads, not against the canvas: the
    /// stitcher touches the previous frame's rectangle (≈0.5 MB on `real-1`)
    /// and the overlay displays at most one screenful, so a budget a few times
    /// that never evicts a tile the capture is still using. It is what bounds
    /// the footprint of a capture that can be arbitrarily long.
    pub fn default_budget(frame_bytes: usize) -> usize {
        (frame_bytes.saturating_mul(4)).max(4 << 20)
    }

    /// A store for test fixtures that need a canvas without going through the
    /// append path.
    #[cfg(test)]
    pub(crate) fn for_tests(width: u32, stride: u32) -> Self {
        Self::new(width, stride, 0, 0)
    }

    pub(super) fn new(width: u32, stride: u32, row: i32, budget: usize) -> Self {
        Self {
            width,
            stride,
            origin_row: row,
            tiles: RefCell::new(Vec::new()),
            written: RefCell::new(Vec::new()),
            compressor: new_tile_compressor(),
            budget,
            resident_bytes: Cell::new(0),
            packed_bytes: Cell::new(0),
            clock: Cell::new(0),
            touches_since_sweep: Cell::new(0),
        }
    }

    pub(super) fn width(&self) -> u32 {
        self.width
    }

    pub(super) fn stride(&self) -> u32 {
        self.stride
    }

    pub(super) fn origin_row(&self) -> i32 {
        self.origin_row
    }

    /// Canvas row of the last row the tile grid can address (exclusive).
    pub(super) fn grid_end_row(&self) -> i32 {
        self.origin_row + (self.tiles.borrow().len() as i32) * CANVAS_TILE_ROWS as i32
    }

    pub(super) fn report(&self) -> CanvasMemoryReport {
        let tiles = self.tiles.borrow();
        let mut report = CanvasMemoryReport {
            logical_bytes: self.stride as usize * tiles.len() * CANVAS_TILE_ROWS as usize,
            codec: self.compressor.name(),
            ..Default::default()
        };
        for tile in tiles.iter() {
            match tile {
                Tile::Empty => report.empty_tiles += 1,
                Tile::Resident { .. } => report.resident_tiles += 1,
                Tile::Packed { .. } => report.packed_tiles += 1,
            }
        }
        report.resident_bytes = self.resident_bytes.get();
        report.packed_bytes = self.packed_bytes.get();
        report
    }

    /// Tile index and row within that tile for canvas row `y`.
    ///
    /// Both are relative to the grid's anchor, not to canvas row 0: the anchor
    /// moves by whole bands when the grid grows upward and by any amount when
    /// the whole canvas is relabelled, so a band's rows are only known relative
    /// to it.
    fn locate(&self, y: i32) -> Option<(usize, usize)> {
        let relative = y.checked_sub(self.origin_row)?;
        if relative < 0 {
            return None;
        }
        let band = CANVAS_TILE_ROWS as i32;
        let index = (relative / band) as usize;
        if index >= self.tiles.borrow().len() {
            return None;
        }
        Some((index, (relative % band) as usize))
    }

    /// Grows the grid downward until it can address `bottom` (exclusive).
    ///
    /// Growth is cheap: only empty `Tile` headers are pushed, and a band that is
    /// never written costs no pixels.
    pub(super) fn ensure_bottom(&mut self, bottom: i32) {
        while self.grid_end_row() < bottom {
            self.tiles.borrow_mut().push(Tile::Empty);
            let mut written = self.written.borrow_mut();
            written.resize(self.tiles.borrow().len() * CANVAS_TILE_ROWS as usize, false);
        }
    }

    /// Grows the grid upward until it can address `top`.
    pub(super) fn ensure_top(&mut self, top: i32) {
        let band = CANVAS_TILE_ROWS as i32;
        let mut grown = false;
        while self.origin_row > top {
            self.origin_row -= band;
            self.tiles.borrow_mut().insert(0, Tile::Empty);
            grown = true;
        }
        if grown {
            let mut written = self.written.borrow_mut();
            let rows = band as usize * self.tiles.borrow().len();
            let already = written.len();
            let missing = rows.saturating_sub(already);
            written.splice(0..0, std::iter::repeat_n(false, missing));
            written.resize(rows, false);
        }
    }

    /// Relabels every row by `delta`, without touching a byte of pixel data.
    ///
    /// This is what growing the canvas upward needs. The pixel canvas re-bases
    /// itself so its row 0 becomes the new top, which means old row `r` is now
    /// row `r + |min_y|`: a uniform shift of the whole canvas. Shifting the
    /// grid's anchor by the same amount relabels every band at once — the
    /// alternative, re-cutting bands so they line up with the new row 0, would
    /// copy every pixel of the canvas on every prepend (which is what the
    /// contiguous buffer did, and why it peaked at twice the canvas).
    pub(super) fn relabel(&mut self, delta: i32) {
        // The per-row "written" flags are indexed relative to `origin_row`, so
        // they move with it and need no rewriting.
        self.origin_row += delta;
    }

    /// Discards everything and starts over at `width`/`stride`, which is what a
    /// width change needs: the row length changes, so no band survives.
    pub(super) fn rebuild(&mut self, width: u32, stride: u32, row: i32) {
        self.width = width;
        self.stride = stride;
        self.origin_row = row;
        self.tiles.borrow_mut().clear();
        self.written.borrow_mut().clear();
        self.resident_bytes.set(0);
        self.packed_bytes.set(0);
        self.clock.set(0);
    }

    /// A whole row, borrowed while `f` runs.
    ///
    /// Closure-based rather than returning a reference: unpacking a cold tile
    /// mutates the working set, and the store must be readable through a shared
    /// reference (`PlacementRequest` holds one). `f` must not call back into the
    /// same store — every accessor below is written to respect that.
    ///
    /// `None` means the row was never captured, which is different from a row of
    /// zeros. A tile is allocated whole, so a band can hold rows no frame ever
    /// wrote; those read as missing, exactly as they did when the canvas was a
    /// buffer and anything past its end was out of bounds.
    pub(super) fn with_row<R>(&self, y: i32, f: impl FnOnce(&[u8]) -> R) -> Option<R> {
        let (index, row_in_tile) = self.locate(y)?;
        if !self.is_written(y) {
            return None;
        }
        self.touch_tile(index)?;
        let offset = row_in_tile * self.stride as usize;
        let end = offset + self.stride as usize;
        let tiles = self.tiles.borrow();
        let Tile::Resident { data, .. } = tiles.get(index)? else {
            return None;
        };
        (end <= data.len()).then(|| f(&data[offset..end]))
    }

    /// Marks a tile as just used, unpacking it if it has gone cold.
    ///
    /// The tile borrow is released before eviction runs: eviction needs the
    /// whole tile list, and `RefCell` would panic on the overlapping borrows.
    fn touch_tile(&self, index: usize) -> Option<()> {
        let clock = self.clock.get() + 1;
        self.clock.set(clock);
        {
            let mut tiles = self.tiles.borrow_mut();
            let tile = tiles.get_mut(index)?;
            match tile {
                Tile::Empty => return None,
                Tile::Resident { used, .. } => *used = clock,
                Tile::Packed { data, len } => {
                    let bytes = data.len();
                    let mut unpacked = vec![0u8; *len];
                    if !self.compressor.unpack(data, &mut unpacked) {
                        return None;
                    }
                    self.packed_bytes.set(self.packed_bytes.get().saturating_sub(bytes));
                    self.resident_bytes.set(self.resident_bytes.get() + unpacked.len());
                    *tile = Tile::Resident { data: unpacked, used: clock };
                }
            }
        }
        self.evict_if_needed(Some(index));
        Some(())
    }

    /// Packs the least-recently-used resident tiles until the working set fits
    /// the budget. `keep` is never evicted (it is the tile being used now).
    /// Packs the least-recently-used resident tiles until the working set fits
    /// the budget. `keep` is never evicted (it is the tile the caller is about
    /// to read).
    fn evict_if_needed(&self, keep: Option<usize>) {
        const EVICT_INTERVAL: u32 = 32;
        if self.budget == 0 || !self.compressor.packs() {
            return;
        }
        let touches = self.touches_since_sweep.get() + 1;
        if self.resident_bytes.get() <= self.budget {
            self.touches_since_sweep.set(0);
            return;
        }
        if touches < EVICT_INTERVAL {
            self.touches_since_sweep.set(touches);
            return;
        }
        self.touches_since_sweep.set(0);
        // Take it back to three quarters, not to the budget exactly: the next
        // sweep then has room and the sort stays rare.
        let target = self.budget - self.budget / 4;
        let mut tiles = self.tiles.borrow_mut();
        // Least recently used first. The list is rebuilt each time rather than
        // maintained: eviction runs only when the budget is exceeded, and it
        // only has to find candidates.
        let mut order: Vec<(u64, usize)> = tiles
            .iter()
            .enumerate()
            .filter_map(|(index, tile)| match tile {
                Tile::Resident { used, .. } => Some((*used, index)),
                _ => None,
            })
            .collect();
        order.sort_unstable();
        for (_, index) in order {
            if self.resident_bytes.get() <= target {
                break;
            }
            if keep == Some(index) {
                continue;
            }
            let tile = &mut tiles[index];
            let Tile::Resident { data, .. } = tile else {
                continue;
            };
            let resident = data.len();
            let packed = self.compressor.pack(data);
            let packed_len = packed.len();
            if packed_len >= resident {
                // Incompressible: keep it rather than pay a copy for nothing.
                continue;
            }
            *tile = Tile::Packed { data: packed, len: resident };
            self.resident_bytes.set(self.resident_bytes.get() - resident);
            self.packed_bytes.set(self.packed_bytes.get() + packed_len);
        }
    }

    /// Folds every captured row into `hash`, unpacking as it goes.
    pub(super) fn with_row_digest(&self, hash: &mut u64) -> Option<()> {
        // Read the count out first: `for i in self.tiles.borrow().len()..` keeps
        // the borrow alive for the whole loop body, and the body borrows again.
        let tile_count = self.tiles.borrow().len() as i32;
        for index in 0..tile_count {
            let base = self.origin_row + index * CANVAS_TILE_ROWS as i32;
            for row in 0..CANVAS_TILE_ROWS as i32 {
                self.with_row(base + row, |bytes| {
                    for byte in bytes {
                        *hash ^= u64::from(*byte);
                        *hash = hash.wrapping_mul(0x100_0000_01b3);
                    }
                });
            }
        }
        Some(())
    }

    /// Whether a frame ever wrote this row.
    pub(super) fn is_written(&self, y: i32) -> bool {
        let Ok(relative) = usize::try_from(y - self.origin_row) else {
            return false;
        };
        self.written.borrow().get(relative).copied().unwrap_or(false)
    }

    pub(super) fn pixel(&self, x: u32, y: i32) -> Option<[u8; 3]> {
        if x >= self.width {
            return None;
        }
        let offset = x as usize * 3;
        self.with_row(y, |row| [row[offset], row[offset + 1], row[offset + 2]])
    }

    /// Copies `[x, x + width)` of row `y` into `dst[..len]`.
    pub(super) fn row_into(&self, x: u32, y: i32, dst: &mut [u8]) -> bool {
        let offset = x as usize * 3;
        let len = dst.len();
        self.with_row(y, |row| {
            if offset + len > row.len() {
                return false;
            }
            dst.copy_from_slice(&row[offset..offset + len]);
            true
        })
        .unwrap_or(false)
    }

    /// Copies a rectangle out, row by row. `dst_stride` is `dst`'s row length.
    pub(super) fn region_into(
        &self,
        x: u32,
        y: i32,
        width: u32,
        height: u32,
        dst: &mut [u8],
        dst_stride: u32,
    ) -> bool {
        for row in 0..height as i32 {
            let dst_start = row as usize * dst_stride as usize;
            let dst_end = dst_start + width as usize * 3;
            if dst_end > dst.len() {
                return false;
            }
            if !self.row_into(x, y + row, &mut dst[dst_start..dst_end]) {
                return false;
            }
        }
        true
    }

    /// Writes column spans of one canvas row from `src`, whose row `src_row`
    /// starts at `src_x`. Spans are `[start, end)` in canvas columns.
    ///
    /// This is the only write primitive: the append path above it keeps owning
    /// the decision of *what* to write (uncovered strips, overlap rows, whole
    /// frame rows) and never touches tile layout.
    pub(super) fn write_spans(
        &self,
        y: i32,
        spans: &[(u32, u32)],
        src: &[u8],
        src_stride: u32,
        src_x: u32,
        src_row: u32,
    ) {
        if spans.is_empty() {
            return;
        }
        let Some((index, row_in_tile)) = self.locate(y) else {
            return;
        };
        if let Ok(relative) = usize::try_from(y - self.origin_row) {
            if let Some(flag) = self.written.borrow_mut().get_mut(relative) {
                *flag = true;
            }
        }
        let row_offset = row_in_tile * self.stride as usize;
        let src_row_start = src_row as usize * src_stride as usize;
        let mut tiles = self.tiles.borrow_mut();
        let Some(tile) = tiles.get_mut(index) else {
            return;
        };
        self.prepare_tile_for_write(tile);
        let Tile::Resident { data, used } = tile else {
            return;
        };
        self.clock.set(self.clock.get() + 1);
        *used = self.clock.get();
        for &(start, end) in spans {
            if end <= start || end > self.width {
                continue;
            }
            let columns = (start..end).filter(|x| x >= &src_x);
            for x in columns {
                let from = src_row_start + (x - src_x) as usize * 3;
                if from + 3 > src.len() {
                    break;
                }
                let to = row_offset + x as usize * 3;
                if to + 3 > data.len() {
                    break;
                }
                data[to..to + 3].copy_from_slice(&src[from..from + 3]);
            }
        }
    }

    /// Makes a tile writable: allocates it if empty, unpacks it if packed.
    fn prepare_tile_for_write(&self, tile: &mut Tile) {
        match tile {
            Tile::Empty => {
                let len = CANVAS_TILE_ROWS as usize * self.stride as usize;
                *tile = Tile::Resident { data: vec![0u8; len], used: 0 };
                self.resident_bytes.set(self.resident_bytes.get() + len);
            }
            Tile::Packed { data, len } => {
                let packed = data.len();
                let mut unpacked = vec![0u8; *len];
                if self.compressor.unpack(data, &mut unpacked) {
                    self.packed_bytes.set(self.packed_bytes.get().saturating_sub(packed));
                    self.resident_bytes.set(self.resident_bytes.get() + unpacked.len());
                    *tile = Tile::Resident { data: unpacked, used: 0 };
                }
            }
            Tile::Resident { .. } => {}
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    /// A page shaped like the content this exists for: a flat background per
    /// row, with sparse per-row marks. No two rows are alike, and it packs the
    /// way real text does (the measured corpora pack to 5–9%).
    ///
    /// A uniformly noisy page would be incompressible by construction, which
    /// would make every packing assertion below vacuous.
    fn page(width: u32, rows: u32) -> Vec<u8> {
        let mut data = Vec::with_capacity((width * 3 * rows) as usize);
        for y in 0..rows {
            let background = 230u8.wrapping_sub((y % 7) as u8);
            for x in 0..width {
                let marked = x % 5 == 0 && (x / 5 + y) % 11 == 0;
                let value = if marked { 20u8.wrapping_add(y as u8) } else { background };
                data.extend_from_slice(&[value, value, value]);
            }
        }
        data
    }

    /// Reads every row once, which is what the stitcher does to a frame's worth
    /// of canvas: the sweep that packs cold tiles runs from the accessor.
    fn read_every_row(store: &CanvasStore, width: u32, rows: u32) {
        let mut out = vec![0u8; width as usize * 3];
        for row in 0..rows as i32 {
            let _ = store.row_into(0, row, &mut out);
        }
    }

    fn store_with(budget: usize, width: u32, rows: u32) -> (CanvasStore, Vec<u8>) {
        let stride = width * 3;
        let source = page(width, rows);
        let mut store = CanvasStore::new(width, stride, 0, budget);
        store.ensure_bottom(rows as i32);
        for row in 0..rows as i32 {
            store.write_spans(row, &[(0, width)], &source, stride, 0, row as u32);
        }
        (store, source)
    }

    #[test]
    fn a_written_row_reads_back_exactly() {
        let (store, source) = store_with(0, 12, 40);
        let stride = 12 * 3;
        for row in 0..40i32 {
            let mut out = vec![0u8; stride as usize];
            assert!(store.row_into(0, row, &mut out), "row {row}");
            let start = row as usize * stride as usize;
            assert_eq!(out, source[start..start + stride as usize], "row {row}");
        }
        assert_eq!(store.pixel(3, 7), {
            let offset = 7 * stride as usize + 3 * 3;
            Some([source[offset], source[offset + 1], source[offset + 2]])
        });
    }

    #[test]
    fn reads_outside_the_written_grid_report_nothing() {
        let (store, _) = store_with(0, 8, 10);
        assert_eq!(store.pixel(0, -1), None);
        let mut out = vec![0u8; 3];
        assert!(!store.row_into(0, 99, &mut out));
        assert_eq!(store.pixel(8, 0), None, "column past the width");
    }

    /// The point of the whole module: a cold tile is packed, and reading it
    /// again unpacks exactly the bytes that went in.
    /// These three assert *packing*; under `canvas-raw-tiles` the codec is a
    /// pass-through by design and there is nothing to pack.
    fn assert_codec_packs() -> bool {
        new_tile_compressor().packs()
    }

    #[test]
    fn a_packed_tile_unpacks_to_the_same_pixels() {
        if !assert_codec_packs() {
            return;
        }
        let width = 64;
        let rows = CANVAS_TILE_ROWS * 4;
        let stride = width * 3;
        let (store, source) = store_with(1, width, rows);
        let before = store.report();
        assert!(before.packed_bytes == 0, "everything is hot before it is read");
        // Reading walks the working set past its budget, which is what packs.
        read_every_row(&store, width, rows);
        let after = store.report();
        assert!(
            after.packed_tiles >= 3,
            "settling must pack the cold tiles, got {:?}",
            after
        );
        assert!(
            after.resident_bytes < before.resident_bytes / 2,
            "packing must free most of the working set: {:?} -> {:?}",
            before,
            after
        );
        for row in 0..rows as i32 {
            let mut out = vec![0u8; stride as usize];
            assert!(store.row_into(0, row, &mut out), "row {row} reads back after packing");
            let start = row as usize * stride as usize;
            assert_eq!(out, source[start..start + stride as usize], "row {row}");
        }
    }

    /// Writing into a packed tile has to unpack it first, or the frame's rows
    /// would land on packed bytes.
    #[test]
    fn a_write_into_a_packed_tile_reads_back_with_both_contents() {
        if !assert_codec_packs() {
            return;
        }
        let width = 32;
        let rows = CANVAS_TILE_ROWS * 2;
        let (store, source) = store_with(1, width, rows);
        read_every_row(&store, width, rows);
        assert!(store.report().packed_tiles > 0);

        let stride = width * 3;
        let patch = vec![9u8; stride as usize * 4];
        for row in 5..9i32 {
            store.write_spans(row, &[(4, 8)], &patch, stride, 4, (row - 5) as u32);
        }
        let mut out = vec![0u8; stride as usize];
        assert!(store.row_into(0, 5, &mut out));
        // Patched columns are 9, the rest is the original page.
        assert_eq!(&out[4 * 3..8 * 3], &patch[0..4 * 3]);
        let start = 5 * stride as usize;
        assert_eq!(&out[0..4 * 3], &source[start..start + 4 * 3]);
        assert_eq!(&out[8 * 3..], &source[start + 8 * 3..start + stride as usize]);
    }

    #[test]
    fn prepending_a_band_moves_the_grid_without_copying_pixels() {
        let (mut store, source) = store_with(0, 16, CANVAS_TILE_ROWS);
        let stride = 16 * 3;
        let resident_before = store.report().resident_bytes;
        // Growing upward: the old content shifts down one band, and the new
        // band above it has never been written.
        store.relabel(CANVAS_TILE_ROWS as i32);
        store.ensure_top(0);
        let resident_after = store.report().resident_bytes;
        assert_eq!(
            resident_before, resident_after,
            "prepending a band must not touch tile bytes"
        );
        // Every row moved down by one band, and the rows above are new.
        let mut out = vec![0u8; stride as usize];
        assert!(store.row_into(0, CANVAS_TILE_ROWS as i32, &mut out));
        assert_eq!(out, source[0..stride as usize], "old row 0 is now one band lower");
        assert_eq!(store.pixel(0, 0), None, "the band above is never-captured");
        assert_eq!(store.grid_end_row(), CANVAS_TILE_ROWS as i32 * 2);
    }

    /// A ragged canvas must not pay for the pixels it never captured: the
    /// contiguous buffer this replaces zeroed the whole bounding box.
    #[test]
    fn never_written_rows_allocate_nothing() {
        let stride = 32 * 3;
        let mut store = CanvasStore::new(32, stride, 0, 0);
        store.ensure_bottom((CANVAS_TILE_ROWS * 8) as i32);
        let report = store.report();
        assert_eq!(report.resident_bytes, 0);
        assert_eq!(report.packed_bytes, 0);
        assert_eq!(report.empty_tiles, 8);
        assert!(report.logical_bytes > 0);
    }

    /// The reported footprint is what the session actually costs, so the memory
    /// claim in `docs/` can be checked rather than trusted.
    #[test]
    fn the_report_accounts_for_every_byte() {
        if !assert_codec_packs() {
            return;
        }
        let width = 40;
        let rows = CANVAS_TILE_ROWS * 3;
        let (store, _) = store_with(1, width, rows);
        read_every_row(&store, width, rows);
        let report = store.report();
        assert_eq!(report.codec, new_tile_compressor().name());
        let measured = report.resident_bytes + report.packed_bytes;
        let tiles = report.resident_tiles + report.packed_tiles + report.empty_tiles;
        assert_eq!(tiles * CANVAS_TILE_ROWS as usize * (width * 3) as usize, report.logical_bytes);
        assert!(
            measured < report.logical_bytes / 2,
            "packed + resident ({measured}) must be far below the logical canvas ({})",
            report.logical_bytes
        );
    }
}

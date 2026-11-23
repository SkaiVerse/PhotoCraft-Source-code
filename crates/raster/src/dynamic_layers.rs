//! Phase 5: Dynamic Layers, 512x512 Lazy Metal Tile Allocation, Empty-Tile Bitmask,
//! Copy-on-Write (CoW) layer duplication, and memory-warning disk eviction.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use photocraft_geom::Rect;

pub const TILE_SIZE_512: u32 = 512;
pub const MIN_DYNAMIC_LAYER_CAP: usize = 8;
pub const MAX_DYNAMIC_LAYER_CAP: usize = 512;
pub const MEMORY_PRESSURE_THRESHOLD: u64 = 256 << 20; // 256 MB

static MEMORY_OVERRIDE: AtomicU64 = AtomicU64::new(0);

/// Set or clear a simulated available memory value for unit tests.
pub fn set_memory_override_for_tests(bytes: Option<u64>) {
    MEMORY_OVERRIDE.store(bytes.unwrap_or(0), Ordering::Relaxed);
}

/// Available device memory in bytes.
/// Uses Darwin's `os_proc_available_memory()` on Apple platforms, system memory measurements
/// on other platforms, or the test override if configured.
pub fn os_proc_available_memory() -> u64 {
    let ov = MEMORY_OVERRIDE.load(Ordering::Relaxed);
    if ov > 0 {
        return ov;
    }
    if let Some(avail) = crate::memory::system::current().available_ram {
        if avail > 0 {
            return avail;
        }
    }
    crate::memory::measure().available_bytes
}

/// Compute the maximum allowed layer count dynamically based on available device memory:
/// clamp(floor(os_proc_available_memory() * 0.5 / bytesPerPaintedLayer), 8, 512)
pub fn compute_dynamic_layer_cap(available_mem: u64, width: u32, height: u32, bpp: usize) -> usize {
    let bytes_per_layer = (width as u64)
        .saturating_mul(height as u64)
        .saturating_mul(bpp as u64);
    if bytes_per_layer == 0 {
        return MAX_DYNAMIC_LAYER_CAP;
    }
    let usable_memory = (available_mem as f64) * 0.5;
    let computed = (usable_memory / (bytes_per_layer as f64)).floor() as i64;
    computed.clamp(MIN_DYNAMIC_LAYER_CAP as i64, MAX_DYNAMIC_LAYER_CAP as i64) as usize
}

/// Compact per-layer bitmask tracking empty vs. populated 512x512 tiles.
/// Each bit corresponds to one 512x512 tile in row-major order:
/// index = row * cols + col.
/// 0 = empty (blank / unallocated), 1 = populated.
#[derive(Clone, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct TileBitmask512 {
    width: u32,
    height: u32,
    cols: u32,
    rows: u32,
    words: Vec<u64>,
}

impl TileBitmask512 {
    pub fn new(width: u32, height: u32) -> Self {
        let cols = width.div_ceil(TILE_SIZE_512);
        let rows = height.div_ceil(TILE_SIZE_512);
        let total_tiles = cols as usize * rows as usize;
        let word_count = total_tiles.div_ceil(64);
        Self {
            width,
            height,
            cols,
            rows,
            words: vec![0u64; word_count],
        }
    }

    #[inline]
    pub fn width(&self) -> u32 {
        self.width
    }

    #[inline]
    pub fn height(&self) -> u32 {
        self.height
    }

    #[inline]
    pub fn cols(&self) -> u32 {
        self.cols
    }

    #[inline]
    pub fn rows(&self) -> u32 {
        self.rows
    }

    /// Fast test: returns true if the layer is completely empty (zero populated tiles).
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|&w| w == 0)
    }

    /// Count of populated 512x512 tiles.
    #[inline]
    pub fn populated_count(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    #[inline]
    fn bit_index(&self, col: u32, row: u32) -> Option<(usize, u64)> {
        if col >= self.cols || row >= self.rows {
            return None;
        }
        let idx = row as usize * self.cols as usize + col as usize;
        Some((idx / 64, 1u64 << (idx % 64)))
    }

    /// Whether the 512x512 tile at (col, row) is populated.
    #[inline]
    pub fn has_tile(&self, col: u32, row: u32) -> bool {
        match self.bit_index(col, row) {
            Some((word_idx, mask)) => self.words.get(word_idx).is_some_and(|w| (w & mask) != 0),
            None => false,
        }
    }

    /// Set or clear the populated state of the tile at (col, row).
    pub fn set_tile(&mut self, col: u32, row: u32, populated: bool) {
        if let Some((word_idx, mask)) = self.bit_index(col, row) {
            if word_idx < self.words.len() {
                if populated {
                    self.words[word_idx] |= mask;
                } else {
                    self.words[word_idx] &= !mask;
                }
            }
        }
    }

    /// Mark all 512x512 tiles intersecting `rect` as populated.
    pub fn mark_rect(&mut self, rect: Rect) {
        if rect.is_empty() || self.cols == 0 || self.rows == 0 {
            return;
        }
        let col_start = (rect.x0.max(0) as u32) / TILE_SIZE_512;
        let col_end = ((rect.x1.max(0) as u32).saturating_sub(1) / TILE_SIZE_512).min(self.cols.saturating_sub(1));
        let row_start = (rect.y0.max(0) as u32) / TILE_SIZE_512;
        let row_end = ((rect.y1.max(0) as u32).saturating_sub(1) / TILE_SIZE_512).min(self.rows.saturating_sub(1));

        for r in row_start..=row_end {
            for c in col_start..=col_end {
                self.set_tile(c, r, true);
            }
        }
    }

    /// Clear all 512x512 tiles intersecting `rect`.
    pub fn clear_rect(&mut self, rect: Rect) {
        if rect.is_empty() || self.cols == 0 || self.rows == 0 {
            return;
        }
        let col_start = (rect.x0.max(0) as u32) / TILE_SIZE_512;
        let col_end = ((rect.x1.max(0) as u32).saturating_sub(1) / TILE_SIZE_512).min(self.cols.saturating_sub(1));
        let row_start = (rect.y0.max(0) as u32) / TILE_SIZE_512;
        let row_end = ((rect.y1.max(0) as u32).saturating_sub(1) / TILE_SIZE_512).min(self.rows.saturating_sub(1));

        for r in row_start..=row_end {
            for c in col_start..=col_end {
                self.set_tile(c, r, false);
            }
        }
    }

    /// Fast test for compositing / dirty-rect traversal:
    /// Returns true if `rect` intersects at least one populated tile.
    pub fn intersects_rect(&self, rect: Rect) -> bool {
        if self.is_empty() || rect.is_empty() || self.cols == 0 || self.rows == 0 {
            return false;
        }
        let col_start = (rect.x0.max(0) as u32) / TILE_SIZE_512;
        let col_end = ((rect.x1.max(0) as u32).saturating_sub(1) / TILE_SIZE_512).min(self.cols.saturating_sub(1));
        let row_start = (rect.y0.max(0) as u32) / TILE_SIZE_512;
        let row_end = ((rect.y1.max(0) as u32).saturating_sub(1) / TILE_SIZE_512).min(self.rows.saturating_sub(1));

        for r in row_start..=row_end {
            for c in col_start..=col_end {
                if self.has_tile(c, r) {
                    return true;
                }
            }
        }
        false
    }

    /// Coordinates of all populated (col, row) tiles.
    pub fn populated_tiles(&self) -> Vec<(u32, u32)> {
        let mut tiles = Vec::new();
        for r in 0..self.rows {
            for c in 0..self.cols {
                if self.has_tile(c, r) {
                    tiles.push((c, r));
                }
            }
        }
        tiles
    }

    /// Bitwise AND of two bitmasks over matching grid dimensions.
    pub fn intersect(&self, other: &Self) -> Self {
        let cols = self.cols.min(other.cols);
        let rows = self.rows.min(other.rows);
        let mut result = Self::new(self.width.min(other.width), self.height.min(other.height));
        for (w_res, (&w_self, &w_other)) in result.words.iter_mut().zip(self.words.iter().zip(&other.words)) {
            *w_res = w_self & w_other;
        }
        let _ = (cols, rows);
        result
    }

    /// Bitwise OR of two bitmasks over matching grid dimensions.
    pub fn union(&self, other: &Self) -> Self {
        let mut result = Self::new(self.width.max(other.width), self.height.max(other.height));
        let max_words = self.words.len().max(other.words.len());
        for i in 0..max_words {
            let s = self.words.get(i).copied().unwrap_or(0);
            let o = other.words.get(i).copied().unwrap_or(0);
            if i < result.words.len() {
                result.words[i] = s | o;
            }
        }
        result
    }
}

/// Metal texture tile storage mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum TileStorageMode {
    /// Allocated as .private Metal texture storage in GPU / unified memory
    Private = 0,
    /// Evicted to compressed temporary storage on disk under memory pressure
    Disk = 1,
    /// Shared staging memory buffer
    Shared = 2,
}

impl TileStorageMode {
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Private,
            1 => Self::Disk,
            _ => Self::Shared,
        }
    }
}

/// Internal cell holding a single 512x512 tile's pixel data and residency state.
pub struct MetalTileCell512 {
    data: RwLock<Option<Vec<u8>>>,
    storage_mode: AtomicU8,
    disk_path: Mutex<Option<PathBuf>>,
    referenced: AtomicBool,
    bpp: usize,
}

impl MetalTileCell512 {
    fn new(bpp: usize) -> Self {
        let byte_len = (TILE_SIZE_512 as usize) * (TILE_SIZE_512 as usize) * bpp;
        Self {
            data: RwLock::new(Some(vec![0u8; byte_len])),
            storage_mode: AtomicU8::new(TileStorageMode::Private as u8),
            disk_path: Mutex::new(None),
            referenced: AtomicBool::new(true),
            bpp,
        }
    }

    fn from_bytes(bytes: Vec<u8>, bpp: usize) -> Self {
        Self {
            data: RwLock::new(Some(bytes)),
            storage_mode: AtomicU8::new(TileStorageMode::Private as u8),
            disk_path: Mutex::new(None),
            referenced: AtomicBool::new(true),
            bpp,
        }
    }

    pub fn bpp(&self) -> usize {
        self.bpp
    }

    pub fn byte_len(&self) -> usize {
        (TILE_SIZE_512 as usize) * (TILE_SIZE_512 as usize) * self.bpp
    }

    pub fn storage_mode(&self) -> TileStorageMode {
        TileStorageMode::from_u8(self.storage_mode.load(Ordering::Relaxed))
    }

    pub fn is_resident(&self) -> bool {
        self.storage_mode() == TileStorageMode::Private
            && self.data.read().unwrap_or_else(PoisonError::into_inner).is_some()
    }

    /// Read the tile's uncompressed pixel bytes, faulting from disk if evicted.
    pub fn read_bytes(&self) -> Result<Vec<u8>, String> {
        self.fault()?;
        self.referenced.store(true, Ordering::Relaxed);
        let g = self.data.read().unwrap_or_else(PoisonError::into_inner);
        g.clone().ok_or_else(|| "tile data not available after fault".to_string())
    }

    /// Evict this tile to an LZ4-compressed temporary file on disk, releasing resident RAM.
    pub fn evict_to_disk(&self, scratch_dir: &Path, tile_id: &str) -> Result<usize, String> {
        let mut w = self.data.write().unwrap_or_else(PoisonError::into_inner);
        let Some(bytes) = w.as_ref() else {
            return Ok(0);
        };
        let packed = lz4_flex::block::compress(bytes);
        let filename = format!("pc_tile_{}_{}.lz4", tile_id, next_tile_serial());
        let path = scratch_dir.join(filename);
        std::fs::write(&path, &packed).map_err(|e| format!("disk eviction write failed: {e}"))?;

        let freed = bytes.len();
        *w = None;
        let mut p = self.disk_path.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(old) = p.take() {
            let _ = std::fs::remove_file(old);
        }
        *p = Some(path);
        self.storage_mode.store(TileStorageMode::Disk as u8, Ordering::Release);
        self.referenced.store(false, Ordering::Relaxed);
        Ok(freed)
    }

    /// Fault an evicted tile back into resident .private Metal texture storage.
    pub fn fault(&self) -> Result<(), String> {
        if self.storage_mode() != TileStorageMode::Disk {
            return Ok(());
        }
        let mut w = self.data.write().unwrap_or_else(PoisonError::into_inner);
        if w.is_some() {
            self.storage_mode.store(TileStorageMode::Private as u8, Ordering::Release);
            return Ok(());
        }
        let path = {
            let p = self.disk_path.lock().unwrap_or_else(PoisonError::into_inner);
            p.clone().ok_or_else(|| "tile marked on disk but disk path missing".to_string())?
        };
        let packed = std::fs::read(&path).map_err(|e| format!("disk fault read failed: {e}"))?;
        let expected_len = self.byte_len();
        let mut decompressed = vec![0u8; expected_len];
        match lz4_flex::block::decompress_into(&packed, &mut decompressed) {
            Ok(n) if n == expected_len => {
                *w = Some(decompressed);
                self.storage_mode.store(TileStorageMode::Private as u8, Ordering::Release);
                let mut p = self.disk_path.lock().unwrap_or_else(PoisonError::into_inner);
                let _ = std::fs::remove_file(&path);
                *p = None;
                Ok(())
            }
            Ok(n) => Err(format!("decompressed size mismatch: got {n}, expected {expected_len}")),
            Err(e) => Err(format!("lz4 decompression failed: {e}")),
        }
    }
}

impl Drop for MetalTileCell512 {
    fn drop(&mut self) {
        if let Some(path) = self.disk_path.get_mut().unwrap_or_else(PoisonError::into_inner).take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

static TILE_SERIAL: AtomicU64 = AtomicU64::new(1);
fn next_tile_serial() -> u64 {
    TILE_SERIAL.fetch_add(1, Ordering::Relaxed)
}

/// A reference-counted 512x512 Metal tile supporting Copy-on-Write (CoW).
#[derive(Clone)]
pub struct MetalTile512 {
    cell: Arc<MetalTileCell512>,
}

impl MetalTile512 {
    pub fn new(bpp: usize) -> Self {
        Self {
            cell: Arc::new(MetalTileCell512::new(bpp)),
        }
    }

    pub fn from_bytes(bytes: Vec<u8>, bpp: usize) -> Self {
        Self {
            cell: Arc::new(MetalTileCell512::from_bytes(bytes, bpp)),
        }
    }

    #[inline]
    pub fn storage_mode(&self) -> TileStorageMode {
        self.cell.storage_mode()
    }

    #[inline]
    pub fn is_resident(&self) -> bool {
        self.cell.is_resident()
    }

    #[inline]
    pub fn read_bytes(&self) -> Result<Vec<u8>, String> {
        self.cell.read_bytes()
    }

    #[inline]
    pub fn evict_to_disk(&self, scratch_dir: &Path, tile_id: &str) -> Result<usize, String> {
        self.cell.evict_to_disk(scratch_dir, tile_id)
    }

    #[inline]
    pub fn fault(&self) -> Result<(), String> {
        self.cell.fault()
    }

    /// Strong reference count of the underlying tile storage.
    #[inline]
    pub fn ref_count(&self) -> usize {
        Arc::strong_count(&self.cell)
    }

    /// Prepare the tile for mutable access. If shared across multiple layer copies (CoW),
    /// clones this specific 512x512 tile to isolate mutations cleanly.
    pub fn make_mut(&mut self) -> Result<(), String> {
        self.cell.fault()?;
        if Arc::strong_count(&self.cell) > 1 {
            let bytes = self.cell.read_bytes()?;
            let bpp = self.cell.bpp;
            self.cell = Arc::new(MetalTileCell512::from_bytes(bytes, bpp));
        }
        Ok(())
    }

    /// Mutate the pixel buffer directly with automatic CoW isolation.
    pub fn with_bytes_mut<R>(&mut self, f: impl FnOnce(&mut [u8]) -> R) -> Result<R, String> {
        self.make_mut()?;
        let mut w = self.cell.data.write().unwrap_or_else(PoisonError::into_inner);
        let bytes = w.as_mut().ok_or_else(|| "tile data missing after fault".to_string())?;
        Ok(f(bytes))
    }
}

/// Dynamic per-layer 512x512 tile storage supporting lazy allocation,
/// compact empty-tile bitmasking, CoW duplication, and disk eviction.
#[derive(Clone)]
pub struct DynamicLayerStorage512 {
    width: u32,
    height: u32,
    bpp: usize,
    bitmask: TileBitmask512,
    tiles: BTreeMap<(u32, u32), MetalTile512>,
}

impl DynamicLayerStorage512 {
    /// Create a new, blank layer storage.
    /// Never pre-allocates tile textures: starts with zero allocated tiles (Lazy allocation).
    pub fn new(width: u32, height: u32, bpp: usize) -> Self {
        Self {
            width,
            height,
            bpp,
            bitmask: TileBitmask512::new(width, height),
            tiles: BTreeMap::new(),
        }
    }

    #[inline]
    pub fn width(&self) -> u32 {
        self.width
    }

    #[inline]
    pub fn height(&self) -> u32 {
        self.height
    }

    #[inline]
    pub fn bpp(&self) -> usize {
        self.bpp
    }

    #[inline]
    pub fn bitmask(&self) -> &TileBitmask512 {
        &self.bitmask
    }

    #[inline]
    pub fn bitmask_mut(&mut self) -> &mut TileBitmask512 {
        &mut self.bitmask
    }

    /// True if the layer contains zero populated tiles.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.bitmask.is_empty()
    }

    /// Number of populated 512x512 tiles.
    #[inline]
    pub fn populated_count(&self) -> usize {
        self.bitmask.populated_count()
    }

    /// Immutable access to a tile at (col, row), if populated.
    pub fn tile(&self, col: u32, row: u32) -> Option<&MetalTile512> {
        if self.bitmask.has_tile(col, row) {
            self.tiles.get(&(col, row))
        } else {
            None
        }
    }

    /// Lazy on-demand allocation & CoW mutation access to a 512x512 tile:
    /// Allocates the tile texture only on first write, and marks the bitmask.
    pub fn ensure_tile_mut(&mut self, col: u32, row: u32) -> Result<&mut MetalTile512, String> {
        if col >= self.bitmask.cols || row >= self.bitmask.rows {
            return Err(format!("tile coord ({col}, {row}) out of bounds ({}, {})", self.bitmask.cols, self.bitmask.rows));
        }
        self.bitmask.set_tile(col, row, true);
        let bpp = self.bpp;
        let tile = self.tiles.entry((col, row)).or_insert_with(|| MetalTile512::new(bpp));
        tile.make_mut()?;
        Ok(tile)
    }

    /// Copy-on-Write (CoW) duplicate:
    /// Shares all underlying 512x512 tile references (reference-counted).
    /// Clones individual tile textures only when either copy mutates a specific tile.
    pub fn duplicate(&self) -> Self {
        Self {
            width: self.width,
            height: self.height,
            bpp: self.bpp,
            bitmask: self.bitmask.clone(),
            tiles: self.tiles.clone(), // BTreeMap clone shares Arc<MetalTileCell512>
        }
    }

    /// Evict cold/resident tiles to compressed temporary disk storage.
    pub fn evict_cold_tiles(&mut self, scratch_dir: &Path, prefix: &str) -> usize {
        let mut freed = 0;
        for (&(c, r), tile) in self.tiles.iter_mut() {
            if tile.is_resident() {
                let tile_id = format!("{prefix}_{c}_{r}");
                if let Ok(n) = tile.evict_to_disk(scratch_dir, &tile_id) {
                    freed += n;
                }
            }
        }
        freed
    }

    /// Fault a tile back from disk into .private Metal texture memory.
    pub fn fault_tile(&self, col: u32, row: u32) -> Result<(), String> {
        if let Some(tile) = self.tiles.get(&(col, row)) {
            tile.fault()
        } else {
            Ok(())
        }
    }

    /// Paint a flat color rectangle into the 512x512 tile grid (lazy allocating tiles touching `rect`).
    pub fn fill_rect(&mut self, rect: Rect, pixel: &[u8]) -> Result<(), String> {
        if rect.is_empty() || self.width == 0 || self.height == 0 {
            return Ok(());
        }
        let col_start = (rect.x0.max(0) as u32) / TILE_SIZE_512;
        let col_end = ((rect.x1.max(0) as u32).saturating_sub(1) / TILE_SIZE_512).min(self.bitmask.cols.saturating_sub(1));
        let row_start = (rect.y0.max(0) as u32) / TILE_SIZE_512;
        let row_end = ((rect.y1.max(0) as u32).saturating_sub(1) / TILE_SIZE_512).min(self.bitmask.rows.saturating_sub(1));

        for r in row_start..=row_end {
            for c in col_start..=col_end {
                let tile_rect = Rect::new(
                    (c * TILE_SIZE_512) as i32,
                    (r * TILE_SIZE_512) as i32,
                    ((c + 1) * TILE_SIZE_512) as i32,
                    ((r + 1) * TILE_SIZE_512) as i32,
                );
                let hit = rect.intersect(&tile_rect);
                if !hit.is_empty() {
                    let bpp = self.bpp;
                    let tile = self.ensure_tile_mut(c, r)?;
                    tile.with_bytes_mut(|bytes| {
                        let tx0 = (hit.x0 - tile_rect.x0) as usize;
                        let ty0 = (hit.y0 - tile_rect.y0) as usize;
                        let tx1 = (hit.x1 - tile_rect.x0) as usize;
                        let ty1 = (hit.y1 - tile_rect.y0) as usize;
                        let stride = (TILE_SIZE_512 as usize) * bpp;
                        for y in ty0..ty1 {
                            let row_off = y * stride;
                            for x in tx0..tx1 {
                                let off = row_off + x * bpp;
                                if off + pixel.len() <= bytes.len() {
                                    bytes[off..off + pixel.len()].copy_from_slice(pixel);
                                }
                            }
                        }
                    })?;
                }
            }
        }
        self.bitmask.mark_rect(rect);
        Ok(())
    }

    /// Calculate the grid tile coordinates covering `rect`.
    pub fn tile_coords_for_rect(&self, rect: Rect) -> Option<(u32, u32, u32, u32)> {
        if rect.is_empty() || self.width == 0 || self.height == 0 || self.bitmask.cols == 0 || self.bitmask.rows == 0 {
            return None;
        }
        let col_start = (rect.x0.max(0) as u32) / TILE_SIZE_512;
        let col_end = ((rect.x1.max(0) as u32).saturating_sub(1) / TILE_SIZE_512).min(self.bitmask.cols.saturating_sub(1));
        let row_start = (rect.y0.max(0) as u32) / TILE_SIZE_512;
        let row_end = ((rect.y1.max(0) as u32).saturating_sub(1) / TILE_SIZE_512).min(self.bitmask.rows.saturating_sub(1));
        if col_start > col_end || row_start > row_end {
            return None;
        }
        Some((col_start, row_start, col_end, row_end))
    }

    /// Capture compressed deltas for all tiles intersecting `rect` prior to mutation.
    /// Only dirty/modified tiles are recorded, respecting lazy unallocated tiles and Phase 5 bitmasks.
    pub fn capture_delta_for_rect(&self, rect: Rect) -> StrokeTileDelta512 {
        let mut deltas = Vec::new();
        if let Some((c0, r0, c1, r1)) = self.tile_coords_for_rect(rect) {
            for r in r0..=r1 {
                for c in c0..=c1 {
                    if let Some(tile) = self.tiles.get(&(c, r)) {
                        let bytes = tile.read_bytes().unwrap_or_default();
                        let compressed = lz4_flex::block::compress(&bytes);
                        deltas.push(CompressedTileDelta512 {
                            col: c,
                            row: r,
                            prior_compressed: Some(compressed),
                        });
                    } else {
                        deltas.push(CompressedTileDelta512 {
                            col: c,
                            row: r,
                            prior_compressed: None,
                        });
                    }
                }
            }
        }
        StrokeTileDelta512 {
            deltas,
            prior_bitmask: self.bitmask.clone(),
        }
    }

    /// Restore the layer storage to the exact prior state captured by `delta` (Undo).
    pub fn restore_delta(&mut self, delta: &StrokeTileDelta512) -> Result<(), String> {
        for d in &delta.deltas {
            match &d.prior_compressed {
                Some(compressed) => {
                    let expected_len = (TILE_SIZE_512 as usize) * (TILE_SIZE_512 as usize) * self.bpp;
                    let decompressed = lz4_flex::block::decompress(compressed, expected_len)
                        .map_err(|e| format!("delta decompression failed: {e}"))?;
                    let tile = MetalTile512::from_bytes(decompressed, self.bpp);
                    self.tiles.insert((d.col, d.row), tile);
                }
                None => {
                    self.tiles.remove(&(d.col, d.row));
                }
            }
        }
        self.bitmask = delta.prior_bitmask.clone();
        Ok(())
    }
}

/// A compressed per-tile delta for undo/redo recording: captures the prior state of modified tiles.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompressedTileDelta512 {
    pub col: u32,
    pub row: u32,
    /// If None, this tile was previously unallocated (blank).
    /// If Some, contains the LZ4-compressed prior bytes of the 512x512 tile.
    pub prior_compressed: Option<Vec<u8>>,
}

/// A collection of compressed tile deltas capturing the exact before/after state of a stroke or edit.
#[derive(Clone, Debug)]
pub struct StrokeTileDelta512 {
    pub deltas: Vec<CompressedTileDelta512>,
    pub prior_bitmask: TileBitmask512,
}

static REGISTERED_STORAGES: Mutex<Vec<std::sync::Weak<Mutex<DynamicLayerStorage512>>>> = Mutex::new(Vec::new());

/// Register a layer storage instance for automatic memory pressure eviction.
pub fn register_layer_storage(storage: &Arc<Mutex<DynamicLayerStorage512>>) {
    let mut reg = REGISTERED_STORAGES.lock().unwrap_or_else(PoisonError::into_inner);
    reg.retain(|w| w.strong_count() > 0);
    reg.push(Arc::downgrade(storage));
}

/// Evict cold/inactive layer tiles across all registered storages to compressed disk storage.
pub fn handle_memory_pressure() -> usize {
    let mut freed = 0;
    // 1. Sweep general raster spill cells to budget
    freed += crate::spill::maintain_budget();

    // 2. Evict registered 512x512 layer storages to scratch disk
    let temp = std::env::temp_dir();
    let reg = {
        let mut g = REGISTERED_STORAGES.lock().unwrap_or_else(PoisonError::into_inner);
        g.retain(|w| w.strong_count() > 0);
        g.clone()
    };
    for weak in reg {
        if let Some(storage_arc) = weak.upgrade() {
            if let Ok(mut storage) = storage_arc.try_lock() {
                freed += storage.evict_cold_tiles(&temp, "press_layer");
            }
        }
    }
    freed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dynamic_layer_cap_clamping_8_to_512() {
        // Zero or negative usable memory clamps to MIN (8)
        assert_eq!(compute_dynamic_layer_cap(0, 4000, 3000, 4), 8);
        assert_eq!(compute_dynamic_layer_cap(10 << 20, 4000, 3000, 4), 8); // 10 MB on 48 MB layer

        // Moderate memory produces proportional layer cap
        // 4 GB available, 2048x2048 RGBA8 layer = 16 MB.
        // usable = 2 GB = 2048 MB -> 2048 / 16 = 128 layers
        let cap = compute_dynamic_layer_cap(4 << 30, 2048, 2048, 4);
        assert_eq!(cap, 128);

        // Huge memory clamps to MAX (512)
        // 64 GB available on small 512x512 RGBA8 (1 MB) -> floor(32 GB / 1 MB) = 32768 -> clamped to 512
        assert_eq!(compute_dynamic_layer_cap(64 << 30, 512, 512, 4), 512);

        // Degenerate canvas size returns MAX
        assert_eq!(compute_dynamic_layer_cap(1 << 30, 0, 0, 4), 512);
        assert_eq!(compute_dynamic_layer_cap(1 << 30, 100, 0, 4), 512);

        // Strict bounds verification
        for mem in [0, 10 << 20, 100 << 20, 1 << 30, 4 << 30, 16 << 30, 128 << 30] {
            for (w, h) in [(0, 0), (100, 100), (1920, 1080), (4096, 4096), (8192, 8192)] {
                let c = compute_dynamic_layer_cap(mem, w, h, 4);
                assert!((8..=512).contains(&c), "Cap {c} must be in [8, 512] for mem={mem}, size={w}x{h}");
            }
        }
    }

    #[test]
    fn test_empty_tile_bitmask_tracking() {
        // 1920x1080 canvas: 4 cols (4 * 512 = 2048) x 3 rows (3 * 512 = 1536) = 12 tiles
        let mut mask = TileBitmask512::new(1920, 1080);
        assert_eq!(mask.cols(), 4);
        assert_eq!(mask.rows(), 3);
        assert!(mask.is_empty());
        assert_eq!(mask.populated_count(), 0);

        // Initially no tiles intersect
        assert!(!mask.intersects_rect(Rect::new(0, 0, 1920, 1080)));
        assert!(!mask.has_tile(0, 0));
        assert!(!mask.has_tile(1, 1));

        // Mark a rect strictly inside tile (1, 1): x in [600, 700], y in [600, 700]
        mask.mark_rect(Rect::new(600, 600, 700, 700));
        assert!(!mask.is_empty());
        assert_eq!(mask.populated_count(), 1);
        assert!(mask.has_tile(1, 1));
        assert!(!mask.has_tile(0, 0));
        assert!(!mask.has_tile(2, 1));

        // Intersection queries
        assert!(mask.intersects_rect(Rect::new(550, 550, 650, 650))); // overlaps tile (1, 1)
        assert!(!mask.intersects_rect(Rect::new(0, 0, 500, 500)));    // tile (0, 0) is empty
        assert!(!mask.intersects_rect(Rect::new(1100, 0, 1500, 500))); // tile (2, 0) is empty

        // Multi-tile rect: spanning across tile boundary (col 0 and col 1)
        mask.mark_rect(Rect::new(400, 100, 600, 200));
        assert!(mask.has_tile(0, 0));
        assert!(mask.has_tile(1, 0));
        assert_eq!(mask.populated_count(), 3); // (1,1), (0,0), (1,0)

        let populated = mask.populated_tiles();
        assert_eq!(populated.len(), 3);
        assert!(populated.contains(&(0, 0)));
        assert!(populated.contains(&(1, 0)));
        assert!(populated.contains(&(1, 1)));
    }

    #[test]
    fn test_cow_tile_isolation_on_mutation() {
        let mut layer_a = DynamicLayerStorage512::new(1024, 1024, 4);
        assert!(layer_a.is_empty());

        // Paint a blue pixel pattern into tile (0, 0)
        let blue = [0u8, 0, 255, 255];
        layer_a.fill_rect(Rect::new(10, 10, 20, 20), &blue).unwrap();
        assert_eq!(layer_a.populated_count(), 1);
        assert!(layer_a.bitmask().has_tile(0, 0));

        let tile_a = layer_a.tile(0, 0).unwrap();
        assert_eq!(tile_a.ref_count(), 1);
        let bytes_a = tile_a.read_bytes().unwrap();
        let off = (10 * 512 + 10) * 4;
        assert_eq!(&bytes_a[off..off + 4], &blue);

        // CoW duplicate layer B from layer A
        let mut layer_b = layer_a.duplicate();
        assert_eq!(layer_b.populated_count(), 1);
        assert!(layer_b.bitmask().has_tile(0, 0));

        // Before mutation, underlying storage is shared (ref_count == 2)
        assert_eq!(layer_a.tile(0, 0).unwrap().ref_count(), 2);
        assert_eq!(layer_b.tile(0, 0).unwrap().ref_count(), 2);

        // Mutate layer B tile (0, 0) with red pixels
        let red = [255u8, 0, 0, 255];
        layer_b.fill_rect(Rect::new(10, 10, 20, 20), &red).unwrap();

        // After mutation, CoW splits the tile: layer A retains blue, layer B has red
        assert_eq!(layer_a.tile(0, 0).unwrap().ref_count(), 1);
        assert_eq!(layer_b.tile(0, 0).unwrap().ref_count(), 1);

        let read_a = layer_a.tile(0, 0).unwrap().read_bytes().unwrap();
        let read_b = layer_b.tile(0, 0).unwrap().read_bytes().unwrap();
        assert_eq!(&read_a[off..off + 4], &blue, "Layer A must remain blue");
        assert_eq!(&read_b[off..off + 4], &red, "Layer B must be updated to red");
    }

    #[test]
    fn test_disk_eviction_and_fault_round_trip_fidelity() {
        let mut storage = DynamicLayerStorage512::new(512, 512, 4);
        let mut test_pattern = vec![0u8; 512 * 512 * 4];
        for (i, b) in test_pattern.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }

        // Populate tile (0, 0)
        let tile = storage.ensure_tile_mut(0, 0).unwrap();
        tile.with_bytes_mut(|bytes| {
            bytes.copy_from_slice(&test_pattern);
        }).unwrap();
        assert_eq!(tile.storage_mode(), TileStorageMode::Private);
        assert!(tile.is_resident());

        // Evict to temporary disk storage
        let temp_dir = std::env::temp_dir();
        let freed = storage.evict_cold_tiles(&temp_dir, "test_evict");
        assert!(freed > 0, "Eviction must free bytes from resident RAM");

        let tile = storage.tile(0, 0).unwrap();
        assert_eq!(tile.storage_mode(), TileStorageMode::Disk);
        assert!(!tile.is_resident());

        // Fault tile back and verify 100% byte fidelity
        tile.fault().unwrap();
        assert_eq!(tile.storage_mode(), TileStorageMode::Private);
        assert!(tile.is_resident());

        let faulted_bytes = tile.read_bytes().unwrap();
        assert_eq!(faulted_bytes, test_pattern, "Faulted tile bytes must match original pattern exactly");
    }

    #[test]
    fn test_compressed_tile_delta_undo_redo_fidelity() {
        let mut storage = DynamicLayerStorage512::new(1024, 1024, 4);
        assert!(storage.is_empty());
        assert_eq!(storage.populated_count(), 0);

        // Step 1: Capture delta of unallocated rect before stroke 1
        let stroke_rect = Rect::new(100, 100, 300, 300); // within tile (0, 0)
        let delta1 = storage.capture_delta_for_rect(stroke_rect);
        assert_eq!(delta1.deltas.len(), 1);
        assert_eq!(delta1.deltas[0].col, 0);
        assert_eq!(delta1.deltas[0].row, 0);
        assert!(delta1.deltas[0].prior_compressed.is_none()); // was unallocated

        // Paint stroke 1 (green)
        let green = [0u8, 255, 0, 255];
        storage.fill_rect(stroke_rect, &green).unwrap();
        assert_eq!(storage.populated_count(), 1);
        assert!(storage.bitmask().has_tile(0, 0));

        // Step 2: Undo stroke 1 using restored delta
        storage.restore_delta(&delta1).unwrap();
        assert!(storage.is_empty(), "Restoring delta1 should return storage to empty");
        assert_eq!(storage.populated_count(), 0);
        assert!(!storage.bitmask().has_tile(0, 0));

        // Step 3: Paint stroke 2 across boundary: tile (0, 0) and (1, 0)
        let boundary_rect = Rect::new(400, 100, 600, 200);
        let delta2 = storage.capture_delta_for_rect(boundary_rect);
        assert_eq!(delta2.deltas.len(), 2);

        let purple = [128u8, 0, 128, 255];
        storage.fill_rect(boundary_rect, &purple).unwrap();
        assert_eq!(storage.populated_count(), 2);
        assert!(storage.bitmask().has_tile(0, 0));
        assert!(storage.bitmask().has_tile(1, 0));

        // Step 4: Duplicate layer (CoW), then mutate layer 2
        let mut storage_dup = storage.duplicate();
        assert_eq!(storage_dup.tile(0, 0).unwrap().ref_count(), 2);

        let dup_rect = Rect::new(450, 120, 550, 180);
        let delta_dup = storage_dup.capture_delta_for_rect(dup_rect);
        let yellow = [255u8, 255, 0, 255];
        storage_dup.fill_rect(dup_rect, &yellow).unwrap();

        // Mutating storage_dup isolated its modified tiles (ref count becomes 1)
        assert_eq!(storage_dup.tile(0, 0).unwrap().ref_count(), 1);
        assert_eq!(storage.tile(0, 0).unwrap().ref_count(), 1);

        // Step 5: Undo on storage_dup restores it to stroke 2 state without affecting storage
        storage_dup.restore_delta(&delta_dup).unwrap();
        let b_dup = storage_dup.tile(0, 0).unwrap().read_bytes().unwrap();
        let b_orig = storage.tile(0, 0).unwrap().read_bytes().unwrap();
        assert_eq!(b_dup, b_orig, "Undo on duplicated layer restores matching pixels");

        // Step 6: Undo on original storage restores it to blank
        storage.restore_delta(&delta2).unwrap();
        assert!(storage.is_empty(), "Original storage returns to blank after undoing delta2");
    }
}

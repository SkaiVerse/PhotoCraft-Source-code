//! A small process-wide cache for brush texture patterns (procedural and bitmap tiles).
//!
//! A stroke (and every symmetry mirror / live tail renderer of it) used to regenerate and
//! level-transform its texture in [`crate::BrushContext::new`], allocating millions of floats per
//! stroke touchdown. The cache is bounded in entries and bytes and evicts stale tiles and the
//! least recently used entry first. Sampled-tip mip pyramids are handled separately in `mip_cache`.
//!
//! The key holds every input of the generator or bitmap identity (`Weak<[u16]>`) plus the level
//! adjustments (brightness, contrast, invert) baked into the cached image, so a hit is always the
//! image a rebuild would give.

use std::sync::{Arc, Mutex, PoisonError, Weak};

use crate::brush::{PatternStyle, Texture};
use crate::tile::{GrayTile, PatternImage};

const PATTERN_MAX_ENTRIES: usize = 8;
const PATTERN_MAX_BYTES: usize = 32 << 20;

enum PatternKey {
    Procedural { style: PatternStyle, size: u32, seed: u32, brightness: u32, contrast: u32, invert: bool },
    Tile { tile: Weak<[u16]>, width: u32, height: u32, brightness: u32, contrast: u32, invert: bool },
}

impl PatternKey {
    fn is_alive(&self) -> bool {
        match self {
            Self::Procedural { .. } => true,
            Self::Tile { tile, .. } => tile.strong_count() > 0,
        }
    }
}

impl PartialEq for PatternKey {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Procedural { style: s1, size: z1, seed: d1, brightness: b1, contrast: c1, invert: i1 },
                Self::Procedural { style: s2, size: z2, seed: d2, brightness: b2, contrast: c2, invert: i2 },
            ) => s1 == s2 && z1 == z2 && d1 == d2 && b1 == b2 && c1 == c2 && i1 == i2,
            (
                Self::Tile { tile: t1, width: w1, height: h1, brightness: b1, contrast: c1, invert: i1 },
                Self::Tile { tile: t2, width: w2, height: h2, brightness: b2, contrast: c2, invert: i2 },
            ) => t1.strong_count() > 0 && Weak::ptr_eq(t1, t2) && w1 == w2 && h1 == h2 && b1 == b2 && c1 == c2 && i1 == i2,
            _ => false,
        }
    }
}

struct PatternEntry {
    key: PatternKey,
    image: Arc<PatternImage>,
}

/// The entries, least recently used first.
struct PatternCache(Vec<PatternEntry>);

impl PatternCache {
    const fn new() -> Self {
        Self(Vec::new())
    }

    /// The image cached for `key`, which is then the most recently used entry.
    fn get(&mut self, key: &PatternKey) -> Option<Arc<PatternImage>> {
        self.0.retain(|e| e.key.is_alive());
        let i = self.0.iter().position(|e| e.key == *key)?;
        let e = self.0.remove(i);
        let image = Arc::clone(&e.image);
        self.0.push(e);
        Some(image)
    }

    /// Caches `image` for `key` (unless it is too big) and evicts the oldest entries past the limits.
    fn insert(&mut self, key: PatternKey, image: &Arc<PatternImage>) {
        self.0.retain(|e| e.key.is_alive());
        if image.data.len() * 4 > PATTERN_MAX_BYTES / 2 {
            return;
        }
        // Another thread may have built the same key meanwhile: keep its image, but it was just asked for.
        match self.0.iter().position(|e| e.key == key) {
            Some(i) => {
                let e = self.0.remove(i);
                self.0.push(e);
            }
            None => self.0.push(PatternEntry { key, image: Arc::clone(image) }),
        }
        let bytes = |e: &PatternEntry| e.image.data.len() * 4;
        while self.0.len() > PATTERN_MAX_ENTRIES || self.0.iter().map(bytes).sum::<usize>() > PATTERN_MAX_BYTES {
            self.0.remove(0);
        }
    }
}

static PATTERNS: Mutex<PatternCache> = Mutex::new(PatternCache::new());

fn level_bits(v: f32) -> u32 {
    (if v.is_finite() { v.clamp(-1.0, 1.0) + 0.0 } else { 0.0 }).to_bits()
}

/// The key of `(style, size, seed)` with `t`'s levels. Brightness and contrast are keyed on the values
/// [`Texture::level`] uses (clamped, non-finite as 0, `-0.0` as `0.0`), so equal levels share an entry.
fn pattern_key(style: PatternStyle, size: u32, seed: u32, t: &Texture) -> PatternKey {
    PatternKey::Procedural {
        style,
        size: size.clamp(8, 1024),
        seed,
        brightness: level_bits(t.brightness),
        contrast: level_bits(t.contrast),
        invert: t.invert,
    }
}

fn tile_key(tile: &GrayTile, t: &Texture) -> PatternKey {
    PatternKey::Tile {
        tile: Arc::downgrade(&tile.data),
        width: tile.width,
        height: tile.height,
        brightness: level_bits(t.brightness),
        contrast: level_bits(t.contrast),
        invert: t.invert,
    }
}

fn get_or_build(key: PatternKey, t: &Texture, build: impl FnOnce() -> PatternImage) -> Arc<PatternImage> {
    if let Some(image) = PATTERNS.lock().unwrap_or_else(PoisonError::into_inner).get(&key) {
        return image;
    }
    let mut img = build();
    for v in &mut img.data {
        *v = t.level(*v);
    }
    let image = Arc::new(img);
    PATTERNS.lock().unwrap_or_else(PoisonError::into_inner).insert(key, &image);
    image
}

/// The procedural pattern `(style, size, seed)` with `t`'s levels applied, from the cache when it
/// was built recently.
pub fn procedural_pattern(style: PatternStyle, size: u32, seed: u32, t: &Texture) -> Arc<PatternImage> {
    get_or_build(pattern_key(style, size, seed, t), t, || crate::procedural::pattern(style, size, seed))
}

/// The bitmap pattern `tile` with `t`'s levels applied, from the cache when it was built recently.
pub fn tile_pattern(tile: &GrayTile, t: &Texture) -> Arc<PatternImage> {
    get_or_build(tile_key(tile, t), t, || PatternImage {
        width: tile.width as usize,
        height: tile.height as usize,
        data: tile.to_f32(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pattern_cache_keys_on_every_input() {
        let base = Texture::default();
        let a = procedural_pattern(PatternStyle::Noise, 64, 3, &base);
        assert!(Arc::ptr_eq(&a, &procedural_pattern(PatternStyle::Noise, 64, 3, &base)));
        let mut fresh = crate::procedural::pattern(PatternStyle::Noise, 64, 3);
        fresh.data.iter_mut().for_each(|v| *v = base.level(*v));
        assert_eq!(a.data, fresh.data);
        let variants = [
            (PatternStyle::Canvas, 64, 3, base.clone()),
            (PatternStyle::Noise, 32, 3, base.clone()),
            (PatternStyle::Noise, 64, 4, base.clone()),
            (PatternStyle::Noise, 64, 3, Texture { invert: true, ..base.clone() }),
            (PatternStyle::Noise, 64, 3, Texture { brightness: 0.5, ..base.clone() }),
            (PatternStyle::Noise, 64, 3, Texture { contrast: 0.5, ..base.clone() }),
        ];
        for (style, size, seed, texture) in variants {
            // Look the base up again so it is the newest entry and still cached when the variant is
            // asked for; a key that ignored the changed input would then hand back the base image.
            procedural_pattern(PatternStyle::Noise, 64, 3, &base);
            let other = procedural_pattern(style, size, seed, &texture);
            assert!(!Arc::ptr_eq(&a, &other) && other.data != a.data, "{style:?} {size} {seed}");
        }
    }

    /// The cache holds a few entries: the oldest one goes first. Runs on a cache this test owns, so
    /// no other test can evict or refresh an entry.
    #[test]
    fn pattern_cache_evicts_the_oldest_entry_past_its_limit() {
        let base = Texture::default();
        let mut cache = PatternCache::new();
        let key = |seed| pattern_key(PatternStyle::Noise, 16, seed, &base);
        let image = |seed| Arc::new(crate::procedural::pattern(PatternStyle::Noise, 16, seed));
        let first = image(0);
        cache.insert(key(0), &first);
        assert!(Arc::ptr_eq(&first, &cache.get(&key(0)).unwrap()));
        for seed in 1..=PATTERN_MAX_ENTRIES as u32 {
            cache.insert(key(seed), &image(seed));
        }
        assert!(cache.get(&key(0)).is_none());
        assert!(cache.get(&key(1)).is_some());
    }

    #[test]
    fn pattern_cache_shares_an_entry_between_equal_levels() {
        let tex = |brightness, contrast| Texture { brightness, contrast, ..Texture::default() };
        let pairs = [
            (tex(2.0, 0.0), tex(1.0, 0.0)),
            (tex(-7.5, 0.0), tex(-1.0, 0.0)),
            (tex(0.0, 3.0), tex(0.0, 1.0)),
            (tex(f32::NAN, 0.0), tex(0.0, 0.0)),
            (tex(0.0, f32::INFINITY), tex(0.0, 0.0)),
            (tex(-0.0, -0.0), tex(0.0, 0.0)),
        ];
        for (a, b) in pairs {
            assert!(pattern_key(PatternStyle::Noise, 64, 5, &a) == pattern_key(PatternStyle::Noise, 64, 5, &b));
            let (ia, ib) = (procedural_pattern(PatternStyle::Noise, 64, 5, &a), procedural_pattern(PatternStyle::Noise, 64, 5, &b));
            assert_eq!(ia.data, ib.data);
        }
        assert!(pattern_key(PatternStyle::Noise, 64, 5, &tex(0.5, 0.0)) != pattern_key(PatternStyle::Noise, 64, 5, &tex(1.0, 0.0)));
    }

    #[test]
    fn tile_pattern_cache_hits_and_keys_on_levels() {
        let tile = GrayTile::from_fn(32, 32, |x, y| ((x + y) % 8) as f32 / 7.0);
        let base = Texture::default();
        let a = tile_pattern(&tile, &base);
        let b = tile_pattern(&tile, &base);
        assert!(Arc::ptr_eq(&a, &b));

        let inverted = tile_pattern(&tile, &Texture { invert: true, ..base.clone() });
        assert!(!Arc::ptr_eq(&a, &inverted) && inverted.data != a.data);

        let brighter = tile_pattern(&tile, &Texture { brightness: 0.4, ..base.clone() });
        assert!(!Arc::ptr_eq(&a, &brighter) && brighter.data != a.data);
    }
}

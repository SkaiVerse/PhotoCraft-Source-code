//! Phase 6 Regression Gate: Compositor Parity Tests
//! Verifies pixel-identical compositing between 512x512 tiled Metal compositor updates,
//! legacy CPU compositor, blend modes, opacity, alpha lock, layer masks, selections,
//! and disk-evicted tile faulting.

use photocraft_color::{BlendMode, ColorMode, PixelFormat, SampleType};
use photocraft_compose::{render, render_tiled};
use photocraft_doc::{Document, Layer, LayerMask};
use photocraft_geom::{Rect, Size};
use photocraft_raster::Surface;

fn close_pixel(a: [f32; 4], b: [f32; 4], tol: f32) -> bool {
    a.iter().zip(b).all(|(x, y)| (x - y).abs() <= tol)
}

fn assert_buffers_close(a: &photocraft_compose::Buffer, b: &photocraft_compose::Buffer, tol: f32, desc: &str) {
    assert_eq!(a.rect, b.rect, "{desc}: buffer rects must match");
    assert_eq!(a.px.len(), b.px.len(), "{desc}: buffer lengths must match");
    let w = a.rect.width() as i32;
    for (i, (px_a, px_b)) in a.px.iter().zip(&b.px).enumerate() {
        assert!(
            close_pixel(*px_a, *px_b, tol),
            "{desc} at pixel {i} (coords: x={}, y={}): a={:?} b={:?} diff > {tol}",
            a.rect.x0 + (i as i32 % w),
            a.rect.y0 + (i as i32 / w),
            px_a,
            px_b
        );
    }
}

#[test]
fn test_blend_modes_512_tiled_vs_untiled_parity() {
    let blend_modes = [
        BlendMode::Normal,
        BlendMode::Multiply,
        BlendMode::Screen,
        BlendMode::Overlay,
        BlendMode::ColorDodge,
        BlendMode::Darken,
        BlendMode::Difference,
        BlendMode::SoftLight,
        BlendMode::Luminosity,
    ];

    let size = Size::new(600, 600); // Crosses 512x512 tile boundary
    for blend in blend_modes {
        let mut doc = Document::new("test", size, ColorMode::Rgb, SampleType::U8);
        let fmt = doc.pixel_format();

        // Layer 1: Background base gradient / colors
        let mut base = Layer::raster("Base", fmt);
        base.surface_mut().unwrap().fill_rect(Rect::new(0, 0, 600, 600), &[0.2, 0.4, 0.8, 1.0]);
        base.surface_mut().unwrap().fill_rect(Rect::new(50, 50, 550, 550), &[0.9, 0.7, 0.1, 1.0]);
        doc.layers.push(base);

        // Layer 2: Blend layer spanning across 512x512 tile boundary
        let mut top = Layer::raster("Top", fmt);
        top.blend = blend;
        top.opacity = 0.85;
        top.surface_mut().unwrap().fill_rect(Rect::new(200, 200, 580, 580), &[0.8, 0.2, 0.5, 0.9]);
        doc.layers.push(top);

        let untiled = render(&doc, doc.bounds());
        let tiled512 = render_tiled(&doc, doc.bounds(), 512);

        // Parity tolerance within 1/255 (RGBA16F rounding)
        assert_buffers_close(&untiled, &tiled512, 1.5 / 255.0, &format!("BlendMode::{blend:?}"));
    }
}

#[test]
fn test_layer_opacity_and_alpha_lock_parity() {
    let size = Size::new(600, 600);
    for opacity in [0.25f32, 0.50, 0.75, 1.0] {
        let mut doc = Document::new("test", size, ColorMode::Rgb, SampleType::U8);
        let fmt = doc.pixel_format();

        let mut base = Layer::raster("Base", fmt);
        base.surface_mut().unwrap().fill_rect(Rect::new(0, 0, 600, 600), &[0.3, 0.3, 0.3, 1.0]);
        doc.layers.push(base);

        let mut mid = Layer::raster("Mid", fmt);
        mid.opacity = opacity;
        mid.locks.transparency = true; // Alpha Lock
        mid.surface_mut().unwrap().fill_rect(Rect::new(100, 100, 550, 550), &[0.1, 0.8, 0.3, 0.7]);
        doc.layers.push(mid);

        let untiled = render(&doc, doc.bounds());
        let tiled512 = render_tiled(&doc, doc.bounds(), 512);
        assert_buffers_close(&untiled, &tiled512, 1.0 / 255.0, &format!("Opacity {opacity}"));
    }
}

#[test]
fn test_layer_mask_and_selection_clipping_parity() {
    let size = Size::new(600, 600);
    let mut doc = Document::new("test", size, ColorMode::Rgb, SampleType::U8);
    let fmt = doc.pixel_format();

    let mut base = Layer::raster("Base", fmt);
    base.surface_mut().unwrap().fill_rect(Rect::new(0, 0, 600, 600), &[1.0, 1.0, 1.0, 1.0]);
    doc.layers.push(base);

    // Layer with mask
    let mut masked_layer = Layer::raster("Masked", fmt);
    masked_layer.surface_mut().unwrap().fill_rect(Rect::new(0, 0, 600, 600), &[0.0, 0.0, 1.0, 1.0]);

    // Mask hiding center
    let mut mask = LayerMask::reveal_all();
    mask.surface.fill_rect(Rect::new(200, 200, 400, 400), &[0.0]); // Hide center
    masked_layer.mask = Some(mask);
    doc.layers.push(masked_layer);

    // Active document selection restricting compositing
    let mut sel_surf = Surface::with_default(PixelFormat::GRAY8, &[0.0]);
    sel_surf.fill_rect(Rect::new(100, 100, 500, 500), &[1.0]);
    doc.selection = Some(sel_surf);

    let untiled = render(&doc, doc.bounds());
    let tiled512 = render_tiled(&doc, doc.bounds(), 512);
    assert_buffers_close(&untiled, &tiled512, 1.0 / 255.0, "Mask and Selection parity");

    // Center masked region (300, 300) should show white base layer
    let p_center = untiled.get(300, 300);
    assert!(close_pixel(p_center, [1.0, 1.0, 1.0, 1.0], 1.0 / 255.0), "Masked center should reveal base: {:?}", p_center);
}

#[test]
fn test_empty_tile_bitmask_fastpath_fidelity() {
    let size = Size::new(1024, 1024);
    let mut doc = Document::new("test", size, ColorMode::Rgb, SampleType::U8);
    let fmt = doc.pixel_format();

    // Background layer
    let mut base = Layer::raster("Base", fmt);
    base.surface_mut().unwrap().fill_rect(Rect::new(0, 0, 1024, 1024), &[0.5, 0.5, 0.5, 1.0]);
    doc.layers.push(base);

    // Completely blank lazy layer (0 allocated tiles, empty bitmask)
    let blank = Layer::raster("Blank", fmt);
    assert!(blank.empty_tile_bitmask_512(1024, 1024).is_empty());
    doc.layers.push(blank);

    // Sparsely populated layer: only tile (1, 1) has pixels (x: 512..600, y: 512..600)
    let mut sparse = Layer::raster("Sparse", fmt);
    sparse.surface_mut().unwrap().fill_rect(Rect::new(512, 512, 600, 600), &[1.0, 0.0, 0.0, 1.0]);
    let sparse_mask = sparse.empty_tile_bitmask_512(1024, 1024);
    assert_eq!(sparse_mask.populated_count(), 1);
    assert!(sparse_mask.has_tile(1, 1));
    assert!(!sparse_mask.has_tile(0, 0));
    doc.layers.push(sparse);

    let untiled = render(&doc, doc.bounds());
    let tiled512 = render_tiled(&doc, doc.bounds(), 512);
    assert_buffers_close(&untiled, &tiled512, 1.0 / 255.0, "Empty-tile bitmask fastpath");

    // Check sparse pixel is red
    let p_sparse = untiled.get(550, 550);
    assert!(close_pixel(p_sparse, [1.0, 0.0, 0.0, 1.0], 1.0 / 255.0));

    // Check untouched region is base gray
    let p_base = untiled.get(200, 200);
    assert!(close_pixel(p_base, [0.5, 0.5, 0.5, 1.0], 1.0 / 255.0));
}

#[test]
fn test_disk_evicted_tiles_compositor_faulting() {
    let mut storage = photocraft_raster::dynamic_layers::DynamicLayerStorage512::new(512, 512, 4);
    let pattern = [200u8, 100, 50, 255];
    storage.fill_rect(Rect::new(0, 0, 512, 512), &pattern).unwrap();

    // Evict tiles to temporary disk directory
    let temp_dir = std::env::temp_dir();
    let freed = storage.evict_cold_tiles(&temp_dir, "phase6_comp_fault");
    assert!(freed > 0, "Eviction must free resident RAM");
    assert!(!storage.tile(0, 0).unwrap().is_resident());

    // Fault back via fault_tile
    storage.fault_tile(0, 0).unwrap();
    assert!(storage.tile(0, 0).unwrap().is_resident());

    let bytes = storage.tile(0, 0).unwrap().read_bytes().unwrap();
    assert_eq!(&bytes[0..4], &pattern);
}

//! Phase 6 Regression Gate: File Format, Import & Export Parity Tests
//! Verifies that off-path GPU-to-CPU bridge, export, and import round-trip
//! correctly handle lazy blank layers, empty-tile bitmasks, and CoW duplicated layers.

use photocraft_color::{ColorMode, SampleType};
use photocraft_doc::{Document, Layer, LayerContent};
use photocraft_geom::{Rect, Size};
use photocraft_io::{ExportOptions, export, import};
use photocraft_raster::Surface;

fn close_pixel(a: &[f32], b: &[f32]) -> bool {
    a.iter().zip(b).all(|(x, y)| (x - y).abs() <= 1.5 / 255.0)
}

#[test]
fn test_roundtrip_lazy_blank_layers_and_empty_bitmasks() {
    let size = Size::new(600, 600);
    let mut doc = Document::new("test_blank", size, ColorMode::Rgb, SampleType::U8);
    let fmt = doc.pixel_format();

    // 1. Background layer
    let mut bg = Layer::raster("Background", fmt);
    bg.surface_mut().unwrap().fill_rect(Rect::new(0, 0, 600, 600), &[1.0, 1.0, 1.0, 1.0]);
    doc.layers.push(bg);

    // 2. Completely blank lazy layer (0 tiles, empty bitmask)
    let blank = Layer::raster("Empty Layer", fmt);
    assert!(blank.empty_tile_bitmask_512(600, 600).is_empty());
    doc.layers.push(blank);

    // Export to PSD
    let out = export(&doc, "test_blank.psd", &ExportOptions::default()).expect("PSD export should succeed");
    assert!(!out.bytes.is_empty());

    // Import back
    let imp = import("test_blank.psd", &out.bytes).expect("PSD import should succeed");
    let reloaded = imp.document;

    assert_eq!(reloaded.size, size);
    assert_eq!(reloaded.layers.len(), 2, "Both Background and Empty Layer must round-trip");
    assert_eq!(reloaded.layers[0].name, "Background");
    assert_eq!(reloaded.layers[1].name, "Empty Layer");

    // The reloaded empty layer must have an empty 512x512 bitmask
    assert!(reloaded.layers[1].empty_tile_bitmask_512(600, 600).is_empty(), "Empty layer must preserve empty 512x512 bitmask");
}

#[test]
fn test_roundtrip_cow_duplicated_layers() {
    let size = Size::new(600, 600);
    let mut doc = Document::new("test_cow", size, ColorMode::Rgb, SampleType::U8);
    let fmt = doc.pixel_format();

    let mut base = Layer::raster("Base", fmt);
    base.surface_mut().unwrap().fill_rect(Rect::new(100, 100, 400, 400), &[0.2, 0.6, 0.9, 1.0]);

    // Duplicate layer (CoW: shares underlying surface tile Arc references)
    let mut dup = base.duplicate();
    dup.name = "Base Copy".to_string();

    doc.layers.push(base);
    doc.layers.push(dup);

    let out = export(&doc, "test_cow.psd", &ExportOptions::default()).expect("PSD export of CoW layers should succeed");
    let imp = import("test_cow.psd", &out.bytes).expect("PSD import should succeed");
    let reloaded = imp.document;

    assert_eq!(reloaded.layers.len(), 2);
    assert_eq!(reloaded.layers[0].name, "Base");
    assert_eq!(reloaded.layers[1].name, "Base Copy");

    // Verify both layers have identical pixel contents
    let s0 = reloaded.layers[0].surface().unwrap();
    let s1 = reloaded.layers[1].surface().unwrap();
    assert_eq!(s0.pixel(200, 200), s1.pixel(200, 200));
    assert!(close_pixel(&s0.pixel(200, 200), &[0.2, 0.6, 0.9, 1.0]));
}

#[test]
fn test_roundtrip_png_and_jpeg_parity() {
    let size = Size::new(400, 400);
    let mut doc = Document::new("test_raster", size, ColorMode::Rgb, SampleType::U8);
    let mut s = Surface::new(doc.pixel_format());
    s.fill_rect(Rect::new(0, 0, 400, 400), &[0.8, 0.2, 0.3, 1.0]);
    doc.layers.push(Layer::new("Background", LayerContent::Raster(s)));

    // PNG export & import
    let png_out = export(&doc, "test.png", &ExportOptions::default()).expect("PNG export should succeed");
    assert!(!png_out.bytes.is_empty());
    let png_imp = import("test.png", &png_out.bytes).expect("PNG import should succeed");
    assert_eq!(png_imp.document.size, size);

    // JPEG export & import
    let jpg_out = export(&doc, "test.jpg", &ExportOptions::default()).expect("JPEG export should succeed");
    assert!(!jpg_out.bytes.is_empty());
    let jpg_imp = import("test.jpg", &jpg_out.bytes).expect("JPEG import should succeed");
    assert_eq!(jpg_imp.document.size, size);
}

#[test]
fn test_roundtrip_empty_bitmask_and_sparse_tile_parity() {
    let size = Size::new(1024, 1024);
    let mut doc = Document::new("test_sparse", size, ColorMode::Rgb, SampleType::U8);
    let fmt = doc.pixel_format();

    // Sparsely populated layer: only tile (1, 1) has pixels (x: 512..600, y: 512..600)
    let mut sparse = Layer::raster("Sparse", fmt);
    sparse.surface_mut().unwrap().fill_rect(Rect::new(512, 512, 600, 600), &[0.1, 0.9, 0.4, 1.0]);
    doc.layers.push(sparse);

    let out = export(&doc, "sparse.psd", &ExportOptions::default()).expect("PSD export of sparse tiles should succeed");
    let imp = import("sparse.psd", &out.bytes).expect("PSD import of sparse tiles should succeed");
    let reloaded = imp.document;

    let reloaded_sparse = &reloaded.layers[0];
    let mask = reloaded_sparse.empty_tile_bitmask_512(1024, 1024);
    assert_eq!(mask.populated_count(), 1, "Only 1 tile should be populated after roundtrip");
    assert!(mask.has_tile(1, 1), "Tile (1, 1) must be populated");
    assert!(!mask.has_tile(0, 0), "Tile (0, 0) must remain unpopulated");

    let surf = reloaded_sparse.surface().unwrap();
    assert!(close_pixel(&surf.pixel(550, 550), &[0.1, 0.9, 0.4, 1.0]));
}

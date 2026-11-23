//! Phase 5: Dynamic layer limit validation and warning generation.

use photocraft_doc::Document;

use crate::{EngineError, Result};

/// Enforces the dynamic layer cap:
/// clamp(floor(os_proc_available_memory() * 0.5 / bytesPerPaintedLayer), 8, 512)
/// Returns a clear, non-destructive error if the document's layer count exceeds the computed cap.
pub fn check_layer_limit(doc: &Document, additional: usize) -> Result<()> {
    let current = doc.layer_count();
    let cap = doc.dynamic_layer_cap();
    if current.saturating_add(additional) > cap {
        return Err(EngineError::Other(format!(
            "Maximum layer limit reached ({cap} layers for {}×{} canvas). Delete unused layers to add more.",
            doc.size.width,
            doc.size.height
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use photocraft_color::{ColorMode, SampleType};
    use photocraft_doc::{Document, Layer, Size};

    use super::*;

    #[test]
    fn test_check_layer_limit_enforcement() {
        // Set test memory to 64 MB
        photocraft_raster::dynamic_layers::set_memory_override_for_tests(Some(64 << 20));

        // 2048x2048 canvas, 4 bytes/pixel = 16 MB / layer.
        // usable = 32 MB -> floor(32 / 16) = 2 layers -> clamped to MIN (8).
        let mut doc = Document::new("test", Size::new(2048, 2048), ColorMode::Rgb, SampleType::U8);
        assert_eq!(doc.dynamic_layer_cap(), 8);

        // Populate 7 layers
        for i in 0..7 {
            doc.layers.push(Layer::raster(format!("L{i}"), doc.pixel_format()));
        }
        assert_eq!(doc.layer_count(), 7);

        // Adding 1 more layer (total 8) succeeds
        assert!(check_layer_limit(&doc, 1).is_ok());

        // Adding 2 more layers (total 9 > 8) fails with clear warning
        let err = check_layer_limit(&doc, 2).unwrap_err().to_string();
        assert!(err.contains("Maximum layer limit reached (8 layers for 2048×2048 canvas)"), "{err}");

        // Now add the 8th layer
        doc.layers.push(Layer::raster("L7", doc.pixel_format()));
        assert_eq!(doc.layer_count(), 8);

        // At the cap, adding any layer fails
        let err = check_layer_limit(&doc, 1).unwrap_err().to_string();
        assert!(err.contains("Maximum layer limit reached (8 layers for 2048×2048 canvas)"), "{err}");

        // Clean up test override
        photocraft_raster::dynamic_layers::set_memory_override_for_tests(None);
    }
}

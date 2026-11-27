//! Procreate brushes (`.brush` and `.brushset`) → PhotoCraft brush presets.
//!
//! Clean-room reader built on `craft-zip` (bounded ZIP archive limits) and `craft-plist` /
//! `craft-xml` (bounded binary & XML property list readers):
//!
//! - `.brush`: a ZIP archive containing `Brush.archive` (an `NSKeyedArchiver` binary plist),
//!   optional raster tip & grain images (`Shape.png` / `*-Shape.*`, `Grain.png` / `*-Grain.*`),
//!   and optional Dual Brush `Sub01/Brush.archive` (+ `Sub01/*Shape*`).
//! - `.brushset`: a ZIP archive containing `brushset.plist` (binary or XML plist with `brushes`
//!   folder UUIDs and optional pack `name`) and one subdirectory per brush.
//!
//! Settings without embedded raster files (`bundledShapePath` / `bundledGrainPath` pointing to
//! Procreate's own app bundle) fall back cleanly with clear entries in [`ProcreateImport::warnings`].

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use craft_plist::{Limits as PlistLimits, Value as PlistValue, parse_binary_with_limits};
use craft_zip::{Archive as ZipArchive, Limits as ZipLimits};
use photocraft_codecs::{ChannelLayout, DecodeOptions, Limits as CodecLimits, SampleType as CodecSample, decode_with};
use photocraft_paint::{
    BrushPreset, BrushSettings, ColorDynamics, Control, DualBrush, Dynamic, GrayTile, MaskMode, Pattern, PatternStyle, ResponseCurve, Scattering, ShapeDynamics,
    Smoothing, Texture, TipShape, Transfer,
};
use photocraft_raster::Interrupt;

use crate::abr_map::{AbrImport, MAX_PATTERN_EDGE, MAX_TIP_EDGE, fit};

/// Archive resource limits for untrusted `.brush` / `.brushset` files.
const MAX_ARCHIVE_ENTRIES: u64 = 4096;
const MAX_ARCHIVE_BYTES: u64 = 512 << 20;
const MAX_PLIST_BYTES: u64 = 16 << 20;
const MAX_IMAGE_FILE_BYTES: u64 = 64 << 20;
const MAX_IMAGE_EDGE: u32 = 4096;
const MAX_NSARCHIVE_DEPTH: usize = 20;
const MAX_BRUSHES_IN_SET: usize = 1024;

/// Output of reading a `.brush` or `.brushset` file (shares [`AbrImport`] shape for UI/command reuse).
pub type ProcreateImport = AbrImport;

/// Parse a `.brush` or `.brushset` archive from raw bytes into presets in `group`.
pub fn read_procreate(bytes: &[u8], group: &str) -> Result<ProcreateImport, String> {
    read_procreate_with(bytes, group, &Interrupt::NONE)
}

/// [`read_procreate`] with cancellation checks and progress reporting.
pub fn read_procreate_with(bytes: &[u8], group: &str, ctl: &Interrupt) -> Result<ProcreateImport, String> {
    ctl.check().map_err(|_| "cancelled".to_string())?;
    let mut zip = ZipArchive::open(bytes, ZipLimits { max_entries: MAX_ARCHIVE_ENTRIES, max_total_bytes: MAX_ARCHIVE_BYTES })
        .map_err(|e| format!("not a readable Procreate brush archive (.brush / .brushset): {e}"))?;
    if zip.has_duplicate_names() {
        return Err("Procreate brush archive contains duplicate entry names".into());
    }
    if zip.has_overlapping_files().map_err(|e| format!("unreadable ZIP archive: {e}"))? {
        return Err("Procreate brush archive contains overlapping entries".into());
    }

    let names: Vec<String> = zip.names().map(|n| n.into_owned()).collect();
    let has_root_archive = has_entry(&names, "Brush.archive");
    let brushset_meta = find_entry(&names, "brushset.plist").map(str::to_string);

    let mut warnings = BTreeSet::new();
    let mut presets = Vec::new();
    let base_group = if group.trim().is_empty() { "Procreate Brushes" } else { group.trim() };

    if has_root_archive && brushset_meta.is_none() {
        ctl.progress(0.2);
        let fallback_name = Path::new(base_group).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "Procreate Brush".into());
        let preset = decode_brush_folder(&mut zip, &names, "", &fallback_name, base_group, &mut warnings)?;
        presets.push(preset);
        ctl.progress(1.0);
        return Ok(ProcreateImport { presets, warnings: warnings.into_iter().collect(), version: 1 });
    }

    // Multi-brush `.brushset` archive: discover brush subdirectories from `brushset.plist` or layout.
    let (pack_name, mut folders) = match brushset_meta {
        Some(plist_path) => {
            let plist_bytes = zip
                .read(&plist_path, MAX_PLIST_BYTES)
                .map_err(|e| format!("brushset.plist: {e}"))?
                .ok_or_else(|| "brushset.plist missing".to_string())?;
            parse_brushset_plist(&plist_bytes)?
        }
        None => (None, Vec::new()),
    };
    if folders.is_empty() {
        folders = discover_brush_folders(&names);
    }
    if folders.is_empty() {
        return Err("archive contains neither Brush.archive nor any brush folders".into());
    }

    let _pack_label = pack_name;
    let total = folders.len().min(MAX_BRUSHES_IN_SET).max(1) as f32;
    for (idx, folder) in folders.into_iter().take(MAX_BRUSHES_IN_SET).enumerate() {
        ctl.check().map_err(|_| "cancelled".to_string())?;
        ctl.progress(0.1 + 0.85 * (idx as f32 / total));
        let prefix = format!("{}/", folder.trim_end_matches('/'));
        if !has_entry(&names, &format!("{prefix}Brush.archive")) {
            warnings.insert(format!("brush folder \"{folder}\" is missing Brush.archive; skipped"));
            continue;
        }
        let fallback_name = format!("Brush {}", idx + 1);
        match decode_brush_folder(&mut zip, &names, &prefix, &fallback_name, base_group, &mut warnings) {
            Ok(preset) => presets.push(preset),
            Err(e) => {
                warnings.insert(format!("skipped brush \"{folder}\": {e}"));
            }
        }
    }
    if presets.is_empty() {
        return Err("no readable Procreate brushes found in archive".into());
    }
    ctl.progress(1.0);
    Ok(ProcreateImport { presets, warnings: warnings.into_iter().collect(), version: 1 })
}

fn has_entry(names: &[String], target: &str) -> bool {
    find_entry(names, target).is_some()
}

fn find_entry<'a>(names: &'a [String], target: &str) -> Option<&'a str> {
    let norm = target.trim_start_matches("./");
    names.iter().find(|n| n.trim_start_matches("./").eq_ignore_ascii_case(norm)).map(String::as_str)
}

fn discover_brush_folders(names: &[String]) -> Vec<String> {
    let mut folders = BTreeSet::new();
    for raw in names {
        let n = raw.trim_start_matches("./");
        if let Some((dir, rest)) = n.split_once('/')
            && !dir.is_empty()
            && !dir.eq_ignore_ascii_case("QuickLook")
            && !dir.eq_ignore_ascii_case("__MACOSX")
            && rest.eq_ignore_ascii_case("Brush.archive")
        {
            folders.insert(dir.to_string());
        }
    }
    folders.into_iter().collect()
}

fn parse_brushset_plist(bytes: &[u8]) -> Result<(Option<String>, Vec<String>), String> {
    if bytes.starts_with(b"bplist") {
        let val = parse_binary_with_limits(bytes, PlistLimits::default()).map_err(|e| format!("brushset.plist: {e}"))?;
        let name = val.get("name").and_then(PlistValue::as_str).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let brushes = val
            .get("brushes")
            .and_then(PlistValue::as_array)
            .map(|arr| arr.iter().filter_map(PlistValue::as_str).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
            .unwrap_or_default();
        return Ok((name, brushes));
    }
    // Apple XML plist (`<plist><dict><key>brushes</key><array><string>...</string></array></dict></plist>`).
    let text = std::str::from_utf8(bytes).map_err(|_| "brushset.plist is not valid UTF-8 XML".to_string())?;
    let root = craft_xml::parse(text.as_bytes()).map_err(|e| format!("brushset.plist XML: {e}"))?;
    let dict = root.child("dict").unwrap_or(&root);
    let children: Vec<_> = dict.elements().collect();
    let mut name = None;
    let mut brushes = Vec::new();
    let mut i = 0;
    while i + 1 < children.len() {
        let key_el = children[i];
        let val_el = children[i + 1];
        if key_el.local() == "key" {
            let k = key_el.text();
            if k == "name" && val_el.local() == "string" {
                let s = val_el.text().trim().to_string();
                if !s.is_empty() {
                    name = Some(s);
                }
            } else if k == "brushes" && val_el.local() == "array" {
                for item in val_el.elements() {
                    if item.local() == "string" {
                        let s = item.text().trim().to_string();
                        if !s.is_empty() {
                            brushes.push(s);
                        }
                    }
                }
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    Ok((name, brushes))
}

fn decode_brush_folder(
    zip: &mut ZipArchive<'_>,
    names: &[String],
    prefix: &str,
    fallback_name: &str,
    group: &str,
    warnings: &mut BTreeSet<String>,
) -> Result<BrushPreset, String> {
    let archive_name = find_entry(names, &format!("{prefix}Brush.archive")).ok_or_else(|| "missing Brush.archive".to_string())?.to_string();
    let plist_bytes = zip
        .read(&archive_name, MAX_PLIST_BYTES)
        .map_err(|e| format!("{archive_name}: {e}"))?
        .ok_or_else(|| format!("{archive_name} missing"))?;
    let dict = parse_keyed_archive(&plist_bytes)?;

    let raw_name = dict.get_str("name").map(str::trim).filter(|s| !s.is_empty()).unwrap_or(fallback_name).to_string();

    let (tip_img, grain_img) = load_folder_textures(zip, names, prefix, &dict, &raw_name, warnings);
    let mut brush = map_brush_dict(&dict, &raw_name, tip_img, grain_img, warnings);

    // Optional Dual Brush in `<prefix>Sub01/Brush.archive`.
    let sub_prefix = format!("{prefix}Sub01/");
    if let Some(sub_archive) = find_entry(names, &format!("{sub_prefix}Brush.archive")).map(str::to_string) {
        if let Ok(Some(sub_bytes)) = zip.read(&sub_archive, MAX_PLIST_BYTES)
            && let Ok(sub_dict) = parse_keyed_archive(&sub_bytes)
        {
            let (sub_tip, _) = load_folder_textures(zip, names, &sub_prefix, &sub_dict, &raw_name, warnings);
            let sub_brush = map_brush_dict(&sub_dict, &raw_name, sub_tip, None, warnings);
            let blend_code = dict.get_i64("dualBlendModeExtended").or_else(|| dict.get_i64("dualBlendMode")).unwrap_or(0);
            brush.dual_brush = DualBrush {
                enabled: true,
                mode: map_mask_mode(blend_code),
                tip: sub_brush.tip,
                size: sub_brush.size,
                hardness: sub_brush.hardness,
                roundness: sub_brush.roundness,
                angle: sub_brush.angle,
                spacing: sub_brush.spacing,
                scatter: sub_brush.shape_scatter,
                both_axes: true,
                count: sub_brush.scattering.count.max(1),
                flip: sub_brush.flip_x || sub_brush.flip_y,
            };
        }
    }

    Ok(BrushPreset { name: raw_name, brush, builtin: false, group: group.to_string(), folder: Vec::new(), captures: Default::default() })
}

fn load_folder_textures(
    zip: &mut ZipArchive<'_>,
    names: &[String],
    prefix: &str,
    dict: &ArchiveDict,
    brush_name: &str,
    warnings: &mut BTreeSet<String>,
) -> (Option<GrayTile>, Option<GrayTile>) {
    let shape_entry = find_texture_entry(names, prefix, dict.get_str("bundledShapePath"), &["shape.png", "shape.jpg", "shape.jpeg"]);
    let grain_entry = find_texture_entry(names, prefix, dict.get_str("bundledGrainPath"), &["grain.png", "grain.jpg", "grain.jpeg"]);

    let tip_tile = match shape_entry {
        Some(entry) => decode_gray_entry(zip, entry, MAX_TIP_EDGE, brush_name, "shape", warnings),
        None => {
            if let Some(bundled) = dict.get_str("bundledShapePath").filter(|s| !s.is_empty() && *s != "$null") {
                warnings.insert(format!(
                    "\"{brush_name}\": Procreate built-in shape \"{bundled}\" is not embedded in the file; a round tip stands in"
                ));
            }
            None
        }
    };
    let grain_tile = match grain_entry {
        Some(entry) => decode_gray_entry(zip, entry, MAX_PATTERN_EDGE, brush_name, "grain", warnings),
        None => {
            if let Some(bundled) = dict.get_str("bundledGrainPath").filter(|s| !s.is_empty() && *s != "$null") {
                warnings.insert(format!(
                    "\"{brush_name}\": Procreate built-in grain \"{bundled}\" is not embedded in the file; a procedural grain stands in"
                ));
            }
            None
        }
    };
    (tip_tile, grain_tile)
}

fn find_texture_entry<'a>(names: &'a [String], prefix: &str, bundled_name: Option<&str>, standard_names: &[&str]) -> Option<&'a str> {
    // 1. Standard Procreate names (`Shape.png`, `Grain.png`, `Reset/Shape.png`, `Reset/Grain.png`).
    for cand in standard_names {
        if let Some(e) = find_entry(names, &format!("{prefix}{cand}")) {
            return Some(e);
        }
        if let Some(e) = find_entry(names, &format!("{prefix}Reset/{cand}")) {
            return Some(e);
        }
    }
    // 2. Exact match against `bundledShapePath` / `bundledGrainPath` inside this folder.
    if let Some(b) = bundled_name.filter(|s| !s.is_empty() && *s != "$null") {
        let base = Path::new(b).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| b.to_string());
        if let Some(e) = find_entry(names, &format!("{prefix}{base}")) {
            return Some(e);
        }
    }
    // 3. Any suffixed PNG/JPG matching `-Shape.*` or `-Grain.*` in the same brush folder (e.g. `GL-Limestone-Shape.png`).
    let kind_suffix = if standard_names.first().is_some_and(|s| s.starts_with("shape")) { "shape." } else { "grain." };
    for raw in names {
        let n = raw.trim_start_matches("./");
        if let Some(rest) = n.strip_prefix(prefix) {
            if rest.contains('/') {
                continue;
            }
            let lower = rest.to_ascii_lowercase();
            if (lower.ends_with(".png") || lower.ends_with(".jpg") || lower.ends_with(".jpeg"))
                && (lower.contains(&format!("-{kind_suffix}")) || lower.contains(&format!("_{kind_suffix}")))
            {
                return Some(raw.as_str());
            }
        }
    }
    None
}

fn decode_gray_entry(
    zip: &mut ZipArchive<'_>,
    entry: &str,
    max_edge: u32,
    brush_name: &str,
    role: &str,
    warnings: &mut BTreeSet<String>,
) -> Option<GrayTile> {
    let bytes = match zip.read(entry, MAX_IMAGE_FILE_BYTES) {
        Ok(Some(b)) => b,
        Ok(None) => return None,
        Err(e) => {
            warnings.insert(format!("\"{brush_name}\": could not read embedded {role} image \"{entry}\": {e}"));
            return None;
        }
    };
    let opts = DecodeOptions {
        limits: CodecLimits { max_width: MAX_IMAGE_EDGE, max_height: MAX_IMAGE_EDGE, max_pixels: u64::from(MAX_IMAGE_EDGE) * u64::from(MAX_IMAGE_EDGE), max_alloc: 128 << 20 },
        ..Default::default()
    };
    let img = match decode_with(&bytes, &opts) {
        Ok(i) => i,
        Err(e) => {
            warnings.insert(format!("\"{brush_name}\": could not decode embedded {role} image \"{entry}\": {e}"));
            return None;
        }
    };
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return None;
    }
    let ga = img.convert(ChannelLayout::GrayA, CodecSample::U16);
    let samples = ga.to_u16_samples().unwrap_or_default();
    let n = (w as usize).saturating_mul(h as usize);
    let mut unit = Vec::with_capacity(n);
    for pair in samples.chunks_exact(2).take(n) {
        let lum = f32::from(pair[0]) / 65535.0;
        let alpha = f32::from(pair[1]) / 65535.0;
        unit.push((lum * alpha).clamp(0.0, 1.0));
    }
    if unit.len() != n {
        return None;
    }
    let (fw, fh, fitted) = fit(w, h, unit, max_edge);
    if (fw, fh) != (w, h) {
        warnings.insert(format!("{role} images larger than {max_edge} px were downsampled"));
    }
    Some(GrayTile::from_f32(fw, fh, &fitted))
}

fn map_brush_dict(
    d: &ArchiveDict,
    name: &str,
    shape_tile: Option<GrayTile>,
    grain_tile: Option<GrayTile>,
    warnings: &mut BTreeSet<String>,
) -> BrushSettings {
    let mut b = BrushSettings::default();
    // Procreate stores normalized `paintSize` (0..1; often ~0.1..1.0). Scale into PhotoCraft pixel sizes cleanly.
    let norm_size = d.get_f32("paintSize").unwrap_or(0.25).clamp(0.01, 4.0);
    b.size = (norm_size * 120.0).clamp(2.0, 2000.0);

    // Hardness / blur (`dynamicsBlur` 0..1 softens computed round tips).
    let blur = d.get_f32("dynamicsBlur").unwrap_or(0.0).clamp(0.0, 1.0);
    b.hardness = (1.0 - blur).clamp(0.0, 1.0);

    // Tip bitmap (white = full paint; invert if `shapeInverted`).
    if let Some(tile) = shape_tile {
        let tile = if d.get_bool("shapeInverted") == Some(true) {
            GrayTile { width: tile.width, height: tile.height, data: tile.data.iter().map(|&px| u16::MAX - px).collect() }
        } else {
            tile
        };
        b.tip = TipShape::Sampled(tile);
    }

    b.opacity = d.get_f32("paintOpacity").unwrap_or(1.0).clamp(0.01, 1.0);
    b.flow = d.get_f32("dynamicsGlazedFlow").unwrap_or(1.0).clamp(0.01, 1.0);

    // Procreate `plotSpacing = 0.0` produces continuous ultra-dense strokes -> floor at `0.05`.
    b.spacing = d.get_f32("plotSpacing").unwrap_or(0.08).max(0.05).clamp(0.01, 10.0);
    b.spacing_enabled = true;

    // Shape properties.
    b.angle = (d.get_f32("shapeRotation").unwrap_or(0.0) + d.get_f32("shapeAngle").unwrap_or(0.0)).rem_euclid(360.0);
    b.roundness = d.get_f32("shapeRoundness").unwrap_or(1.0).clamp(0.01, 1.0);
    b.shape_azimuth = d.get_bool("shapeAzimuth").unwrap_or(false);
    b.shape_scatter = d.get_f32("shapeScatter").unwrap_or(0.0).clamp(0.0, 1.0);

    let count_f = d.get_f32("shapeCount").unwrap_or(0.0).clamp(0.0, 15.0);
    let count_jitter = d.get_f32("shapeCountJitter").unwrap_or(0.0).clamp(0.0, 1.0);
    let plot_jitter = d.get_f32("plotJitter").unwrap_or(0.0).clamp(0.0, 10.0);
    let flip_x = d.get_bool("shapeFlipXJitter").unwrap_or(false);
    let flip_y = d.get_bool("shapeFlipYJitter").unwrap_or(false);
    let jitter_size = d.get_f32("dynamicsJitterSize").unwrap_or(0.0).clamp(0.0, 1.0);
    let jitter_roundness = d.get_f32("jitterShapeRoundness").unwrap_or(0.0).clamp(0.0, 1.0);
    let angle_jitter = if d.get_bool("shapeRandomise").unwrap_or(false) { 1.0 } else { 0.0 };

    if plot_jitter > 0.0 || b.shape_scatter > 0.0 || count_f >= 1.0 || count_jitter > 0.0 {
        b.scattering = Scattering {
            enabled: true,
            scatter: Dynamic::jitter(plot_jitter.max(b.shape_scatter)),
            both_axes: true,
            count: (count_f.round() as u32 + 1).clamp(1, 16),
            count_jitter: Dynamic::jitter(count_jitter),
        };
    }

    if jitter_size > 0.0 || jitter_roundness > 0.0 || flip_x || flip_y || angle_jitter > 0.0 {
        b.shape_dynamics = ShapeDynamics {
            enabled: true,
            size: Dynamic { jitter: jitter_size, minimum: d.get_f32("minSize").unwrap_or(0.0).clamp(0.0, 1.0), ..Dynamic::default() },
            angle: Dynamic { jitter: angle_jitter, control: if b.shape_azimuth { Control::Rotation } else { Control::Off }, ..Dynamic::default() },
            roundness: Dynamic {
                jitter: jitter_roundness,
                minimum: d.get_f32("dynamicsPressureShapeRoundnessMinimum").unwrap_or(0.0).clamp(0.0, 1.0),
                ..Dynamic::default()
            },
            flip_x_jitter: flip_x,
            flip_y_jitter: flip_y,
            ..Default::default()
        };
    }

    // Procreate Pressure, Tilt & Velocity curves and dynamics.
    b.dynamics_pressure_size = d.get_f32("dynamicsPressureSize").unwrap_or(0.0).clamp(0.0, 1.0);
    b.pressure_size = b.dynamics_pressure_size > 0.0;
    if let Some(curve) = d.get_curve("dynamicsPressureSizeCurve") {
        b.dynamics_pressure_size_curve = curve;
    }
    b.dynamics_pressure_opacity = d.get_f32("dynamicsPressureOpacity").unwrap_or(0.0).clamp(0.0, 1.0);
    b.pressure_opacity = b.dynamics_pressure_opacity > 0.0;
    if let Some(curve) = d.get_curve("dynamicsPressureOpacityCurve") {
        b.dynamics_pressure_opacity_curve = curve;
    }
    b.dynamics_tilt_size = d.get_f32("dynamicsTiltSize").unwrap_or(0.0).clamp(0.0, 1.0);
    b.dynamics_tilt_opacity = d.get_f32("dynamicsTiltOpacity").unwrap_or(0.0).clamp(0.0, 1.0);
    b.dynamics_velocity_size = d.get_f32("dynamicsSpeedSize").unwrap_or(0.0).clamp(-1.0, 1.0).abs().clamp(0.0, 1.0);
    b.dynamics_velocity_opacity = d.get_f32("dynamicsSpeedOpacity").unwrap_or(0.0).clamp(-1.0, 1.0).abs().clamp(0.0, 1.0);

    let jitter_opacity = d.get_f32("dynamicsJitterOpacity").unwrap_or(0.0).clamp(0.0, 1.0);
    if jitter_opacity > 0.0 {
        b.transfer = Transfer { enabled: true, opacity: Dynamic::jitter(jitter_opacity), ..Default::default() };
    }

    // Color Dynamics.
    let hue_j = d.get_f32("dynamicsJitterHue").unwrap_or(0.0).clamp(0.0, 1.0);
    let sat_j = d.get_f32("dynamicsJitterSaturation").unwrap_or(0.0).clamp(0.0, 1.0);
    let lit_j = d.get_f32("dynamicsJitterLightness").or_else(|| d.get_f32("dynamicsJitterDarkness")).unwrap_or(0.0).clamp(0.0, 1.0);
    if hue_j > 0.0 || sat_j > 0.0 || lit_j > 0.0 {
        b.color_dynamics = ColorDynamics {
            enabled: true,
            per_tip: true,
            hue_jitter: hue_j,
            saturation_jitter: sat_j,
            brightness_jitter: lit_j,
            ..Default::default()
        };
    }

    // Taper.
    b.taper_start_length = d.get_f32("taperStartLength").unwrap_or(0.0).clamp(0.0, 1.0);
    b.taper_end_length = d.get_f32("taperEndLength").unwrap_or(0.0).clamp(0.0, 1.0);
    b.taper_pressure = d.get_f32("taperPressure").unwrap_or(0.0).clamp(0.0, 1.0);
    b.taper_size = d.get_f32("taperSize").unwrap_or(0.0).clamp(0.0, 1.0);
    b.taper_opacity = d.get_f32("taperOpacity").unwrap_or(0.0).clamp(0.0, 1.0);

    // Grain / Texture.
    let grain_depth = d.get_f32("grainDepth").unwrap_or(1.0).clamp(0.0, 1.0);
    let has_bundled_grain = d.get_str("bundledGrainPath").is_some_and(|s| !s.is_empty() && s != "$null");
    if let Some(tile) = grain_tile {
        let scale = d.get_f32("textureScale").unwrap_or(1.0).clamp(0.01, 10.0);
        let tex_mode = map_mask_mode(d.get_i64("grainBlendModeExtended").or_else(|| d.get_i64("grainBlendMode")).unwrap_or(0));
        b.grain_depth = grain_depth;
        b.texture_scale = scale;
        b.stroke_space_grain = d.get_f32("textureMovement").unwrap_or(1.0) > 0.05;
        b.grain_softness = (1.0 - d.get_f32("textureContrast").unwrap_or(0.0).abs()).clamp(0.15, 1.0);
        b.texture = Texture {
            enabled: grain_depth > 0.0,
            pattern: Pattern::Tile(tile),
            invert: d.get_bool("textureInverted").unwrap_or(false),
            scale,
            brightness: d.get_f32("textureBrightness").unwrap_or(0.0).clamp(-1.0, 1.0),
            contrast: d.get_f32("textureContrast").unwrap_or(0.0).clamp(-1.0, 1.0),
            each_tip: !b.stroke_space_grain,
            mode: tex_mode,
            depth: grain_depth,
            depth_jitter: Dynamic::jitter(d.get_f32("grainDepthJitter").unwrap_or(0.0).clamp(0.0, 1.0)),
        };
    } else if has_bundled_grain && grain_depth > 0.01 {
        let style = match d.get_str("bundledGrainPath").unwrap_or("").to_ascii_lowercase() {
            s if s.contains("canvas") => PatternStyle::Canvas,
            s if s.contains("dot") => PatternStyle::Dots,
            s if s.contains("cloud") || s.contains("noise") => PatternStyle::Noise,
            _ => PatternStyle::Paper,
        };
        let scale = d.get_f32("textureScale").unwrap_or(1.0).clamp(0.01, 10.0);
        b.grain_depth = grain_depth;
        b.texture_scale = scale;
        b.texture = Texture {
            enabled: true,
            pattern: Pattern::Procedural { style, size: 128, seed: 1 },
            invert: d.get_bool("textureInverted").unwrap_or(false),
            scale,
            depth: grain_depth,
            ..Default::default()
        };
    }

    // Stabilization / StreamLine & Wet Edges / Wet Mix.
    let streamline = d.get_f32("plotSmoothing").unwrap_or(0.0).clamp(0.0, 1.0);
    let stabilization = d.get_f32("plotMovingAverageStabilization").unwrap_or(0.0).clamp(0.0, 1.0);
    let fft_stab = d.get_f32("plotFFTSmoothingAmount").unwrap_or(0.0).clamp(0.0, 1.0);
    b.smoothing = Smoothing {
        enabled: true,
        amount: streamline.max(stabilization).max(fft_stab),
        stabilization: stabilization.max(fft_stab),
        streamline,
        catch_up_on_end: true,
        ..Smoothing::default()
    };

    b.wet_edges = d.get_f32("wetEdgesAmount").unwrap_or(0.0) > 0.05;
    b.wet_charge = d.get_f32("dynamicsLoad").unwrap_or(0.0).clamp(0.0, 1.0);
    b.wet_mix = d.get_f32("dynamicsMix").unwrap_or(0.0).clamp(0.0, 1.0);
    b.wet_pull = if d.get_f32("dynamicsLoad").unwrap_or(0.0) > 0.0 || d.get_f32("dynamicsMix").unwrap_or(0.0) > 0.0 {
        d.get_f32("dynamicsSmudgeAccumulation").unwrap_or(0.0).clamp(0.0, 1.0)
    } else {
        0.0
    };

    if b.size < 2.0 || !b.size.is_finite() {
        warnings.insert(format!("\"{name}\": adjusted tiny brush size to minimum 2 px"));
        b.size = 2.0;
    }
    b
}

fn map_mask_mode(code: i64) -> MaskMode {
    match code {
        1 | 19 | 34 => MaskMode::Multiply,
        4 => MaskMode::ColorBurn,
        7 => MaskMode::Darken,
        11 => MaskMode::Overlay,
        15 => MaskMode::ColorDodge,
        _ => MaskMode::Multiply,
    }
}

// ------------------------------------------------------------------ NSKeyedArchiver decoding

#[derive(Debug)]
struct ArchiveDict {
    objs: Vec<PlistValue>,
    root_idx: usize,
}

impl ArchiveDict {
    fn root_entries(&self) -> Option<&[(String, PlistValue)]> {
        match self.objs.get(self.root_idx)? {
            PlistValue::Dict(d) => Some(d),
            _ => None,
        }
    }

    fn resolve<'a>(&'a self, mut v: &'a PlistValue) -> Option<&'a PlistValue> {
        let mut seen = HashSet::new();
        for _ in 0..MAX_NSARCHIVE_DEPTH {
            match v {
                PlistValue::Uid(u) => {
                    let idx = usize::try_from(*u).ok()?;
                    if !seen.insert(idx) {
                        return None;
                    }
                    v = self.objs.get(idx)?;
                }
                _ => return Some(v),
            }
        }
        None
    }

    fn get(&self, key: &str) -> Option<&PlistValue> {
        let entries = self.root_entries()?;
        let raw = entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)?;
        self.resolve(raw)
    }

    fn get_str(&self, key: &str) -> Option<&str> {
        let v = self.get(key)?;
        match v {
            PlistValue::String(s) => Some(s.as_str()),
            PlistValue::Dict(d) => d.iter().find(|(k, _)| k == "NS.string").and_then(|(_, sv)| self.resolve(sv)).and_then(PlistValue::as_str),
            _ => None,
        }
    }

    fn get_bool(&self, key: &str) -> Option<bool> {
        match self.get(key)? {
            PlistValue::Bool(b) => Some(*b),
            PlistValue::Integer(i) => Some(*i != 0),
            _ => None,
        }
    }

    fn get_f32(&self, key: &str) -> Option<f32> {
        match self.get(key)? {
            PlistValue::Real(f) if f.is_finite() => Some(*f as f32),
            PlistValue::Integer(i) => Some(*i as f32),
            _ => None,
        }
    }

    fn get_i64(&self, key: &str) -> Option<i64> {
        match self.get(key)? {
            PlistValue::Integer(i) => i64::try_from(*i).ok(),
            PlistValue::Real(f) if f.is_finite() => Some(*f as i64),
            _ => None,
        }
    }

    fn get_curve(&self, key: &str) -> Option<ResponseCurve> {
        let v = self.get(key)?;
        let raw_pts = v.get("points").and_then(|pv| self.resolve(pv))?;
        let arr = match raw_pts {
            PlistValue::Array(a) => a.as_slice(),
            PlistValue::Dict(d) => d.iter().find(|(k, _)| k == "NS.objects").and_then(|(_, ov)| self.resolve(ov)).and_then(PlistValue::as_array)?,
            _ => return None,
        };
        let mut pts = Vec::with_capacity(arr.len().min(16));
        for item in arr {
            let Some(item) = self.resolve(item) else { continue };
            let s = match item {
                PlistValue::String(s) => Some(s.as_str()),
                PlistValue::Dict(d) => d.iter().find(|(k, _)| k == "NS.string").and_then(|(_, sv)| self.resolve(sv)).and_then(PlistValue::as_str),
                _ => None,
            };
            if let Some(pt) = s.and_then(parse_cgpoint) {
                pts.push(pt);
            }
        }
        if pts.len() < 2 {
            return None;
        }
        pts.sort_by(|a, b| a[0].total_cmp(&b[0]));
        pts.dedup_by(|a, b| (a[0] - b[0]).abs() < 1e-4);
        if pts.len() < 2 {
            return None;
        }
        pts.truncate(16);
        Some(ResponseCurve::new(&pts))
    }
}

fn parse_cgpoint(s: &str) -> Option<[f32; 2]> {
    let trimmed = s.trim().strip_prefix('{')?.strip_suffix('}')?;
    let (xs, ys) = trimmed.split_once(',')?;
    let x: f32 = xs.trim().parse().ok()?;
    let y: f32 = ys.trim().parse().ok()?;
    if x.is_finite() && y.is_finite() { Some([x.clamp(0.0, 1.0), y.clamp(0.0, 1.0)]) } else { None }
}

fn parse_keyed_archive(bytes: &[u8]) -> Result<ArchiveDict, String> {
    let top = parse_binary_with_limits(bytes, PlistLimits::default()).map_err(|e| format!("invalid Brush.archive plist: {e}"))?;
    let PlistValue::Dict(top_dict) = &top else {
        return Err("Brush.archive top-level plist must be a dictionary".into());
    };
    let Some(objs) = top_dict.iter().find(|(k, _)| k == "$objects").and_then(|(_, v)| v.as_array()).map(|a| a.to_vec()) else {
        // Non-keyed flat plist fallback.
        return Ok(ArchiveDict { objs: vec![top], root_idx: 0 });
    };
    let root_uid = top_dict
        .iter()
        .find(|(k, _)| k == "$top")
        .and_then(|(_, v)| v.get("root"))
        .and_then(PlistValue::as_uid)
        .and_then(|u| usize::try_from(u).ok())
        .or_else(|| (objs.len() > 1).then_some(1))
        .ok_or_else(|| "Brush.archive is missing root object".to_string())?;
    let _check_map: HashMap<usize, ()> = HashMap::new();
    Ok(ArchiveDict { objs, root_idx: root_uid })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal valid binary plist encoder (`bplist00`) for unit-testing NSKeyedArchiver inputs cleanly.
    fn write_bplist_objects(objs: &[PlistValue], root_uid: u64) -> Vec<u8> {
        let mut buf = b"bplist00".to_vec();
        let mut flat: Vec<PlistValue> = Vec::new();
        // Reserve 0..3 for top/root_dict/objects_arr
        flat.push(PlistValue::Null);
        flat.push(PlistValue::Dict(vec![("root".into(), PlistValue::Uid(3 + root_uid))]));
        flat.push(PlistValue::Null);
        // Append caller objects starting at index 3 (caller UIDs are 0-based into `objs`, shifted +3).
        let base = 3u64;
        fn shift(v: &PlistValue, base: u64) -> PlistValue {
            match v {
                PlistValue::Uid(u) => PlistValue::Uid(u + base),
                PlistValue::Array(a) => PlistValue::Array(a.iter().map(|x| shift(x, base)).collect()),
                PlistValue::Dict(d) => PlistValue::Dict(d.iter().map(|(k, x)| (k.clone(), shift(x, base))).collect()),
                other => other.clone(),
            }
        }
        flat.extend(objs.iter().map(|o| shift(o, base)));
        flat[2] = PlistValue::Array((0..objs.len()).map(|i| PlistValue::Uid(base + i as u64)).collect());
        flat[0] = PlistValue::Dict(vec![
            ("$archiver".into(), PlistValue::String("NSKeyedArchiver".into())),
            ("$objects".into(), PlistValue::Uid(2)),
            ("$top".into(), PlistValue::Uid(1)),
            ("$version".into(), PlistValue::Integer(100_000)),
        ]);

        // Flatten keys and nested values into an append-only table so every Dict/Array ref is an index.
        let mut atoms: Vec<PlistValue> = Vec::new();
        for _ in 0..flat.len() {
            atoms.push(PlistValue::Null);
        }
        fn push_atom(v: &PlistValue, atoms: &mut Vec<PlistValue>) -> u64 {
            match v {
                PlistValue::Uid(u) => *u,
                other => {
                    let idx = atoms.len() as u64;
                    atoms.push(other.clone());
                    idx
                }
            }
        }
        for i in 0..flat.len() {
            atoms[i] = match &flat[i] {
                PlistValue::Dict(entries) => {
                    let mut pairs = Vec::new();
                    for (k, v) in entries {
                        let ku = {
                            let idx = atoms.len() as u64;
                            atoms.push(PlistValue::String(k.clone()));
                            idx
                        };
                        let vu = push_atom(v, &mut atoms);
                        pairs.push((ku, vu));
                    }
                    PlistValue::Array(pairs.into_iter().flat_map(|(k, v)| [PlistValue::Uid(k), PlistValue::Uid(v)]).collect())
                }
                PlistValue::Array(a) => PlistValue::Array(a.iter().map(|v| PlistValue::Uid(push_atom(v, &mut atoms))).collect()),
                other => other.clone(),
            };
            let _ = &flat[i];
        }
        // Mark which indices came from Dict so serialization uses `0xD0` instead of `0xA0`.
        let is_dict: Vec<bool> = (0..atoms.len()).map(|i| i < flat.len() && matches!(flat[i], PlistValue::Dict(_))).collect();
        let ref_size = 2u8;
        let mut offs = Vec::with_capacity(atoms.len());
        fn write_len(marker_hi: u8, n: usize, out: &mut Vec<u8>) {
            if n < 15 {
                out.push(marker_hi | (n as u8));
            } else {
                out.push(marker_hi | 0x0F);
                if n <= u8::MAX as usize {
                    out.push(0x10);
                    out.push(n as u8);
                } else {
                    out.push(0x11);
                    out.extend_from_slice(&(n as u16).to_be_bytes());
                }
            }
        }
        for (i, a) in atoms.iter().enumerate() {
            offs.push(buf.len() as u64);
            match a {
                PlistValue::Null => buf.push(0x00),
                PlistValue::Bool(false) => buf.push(0x08),
                PlistValue::Bool(true) => buf.push(0x09),
                PlistValue::Integer(n) => {
                    buf.push(0x13);
                    buf.extend_from_slice(&(*n as i64).to_be_bytes());
                }
                PlistValue::Real(f) => {
                    buf.push(0x23);
                    buf.extend_from_slice(&f.to_be_bytes());
                }
                PlistValue::String(s) => {
                    write_len(0x50, s.len(), &mut buf);
                    buf.extend_from_slice(s.as_bytes());
                }
                PlistValue::Uid(u) => {
                    buf.push(0x81);
                    buf.extend_from_slice(&(*u as u16).to_be_bytes());
                }
                PlistValue::Array(items) if is_dict[i] => {
                    let count = items.len() / 2;
                    write_len(0xD0, count, &mut buf);
                    for k in items.iter().step_by(2) {
                        let PlistValue::Uid(u) = k else { continue };
                        buf.extend_from_slice(&(*u as u16).to_be_bytes());
                    }
                    for v in items.iter().skip(1).step_by(2) {
                        let PlistValue::Uid(u) = v else { continue };
                        buf.extend_from_slice(&(*u as u16).to_be_bytes());
                    }
                }
                PlistValue::Array(items) => {
                    write_len(0xA0, items.len(), &mut buf);
                    for v in items {
                        let PlistValue::Uid(u) = v else { continue };
                        buf.extend_from_slice(&(*u as u16).to_be_bytes());
                    }
                }
                _ => buf.push(0x00),
            }
        }
        let table_off = buf.len() as u64;
        for o in &offs {
            buf.extend_from_slice(&(*o as u32).to_be_bytes());
        }
        buf.extend_from_slice(&[0u8; 6]);
        buf.push(4);
        buf.push(ref_size);
        buf.extend_from_slice(&(atoms.len() as u64).to_be_bytes());
        buf.extend_from_slice(&0u64.to_be_bytes());
        buf.extend_from_slice(&table_off.to_be_bytes());
        let _ = 0u64;
        buf
    }

    fn png_gray(w: u32, h: u32, fill: u8) -> Vec<u8> {
        let img = photocraft_codecs::Image::from_u8(w, h, ChannelLayout::Gray, vec![fill; (w * h) as usize]).unwrap();
        photocraft_codecs::encode(&img, photocraft_codecs::Format::Png, &Default::default()).unwrap()
    }

    fn zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut zw = craft_zip::Writer::new();
        for (name, data) in entries {
            zw.add(name, data, craft_zip::Compression::Deflated).unwrap();
        }
        zw.finish().unwrap()
    }

    #[test]
    fn imports_single_brush_with_shape_grain_curves_and_dual_brush() {
        let archive = write_bplist_objects(
            &[
                PlistValue::String("$null".into()),
                PlistValue::Dict(vec![
                    ("name".into(), PlistValue::Uid(2)),
                    ("paintSize".into(), PlistValue::Real(0.5)),
                    ("paintOpacity".into(), PlistValue::Real(0.8)),
                    ("plotSpacing".into(), PlistValue::Real(0.18)),
                    ("dynamicsPressureSize".into(), PlistValue::Real(0.9)),
                    ("dynamicsPressureSizeCurve".into(), PlistValue::Uid(3)),
                    ("dynamicsTiltSize".into(), PlistValue::Real(0.45)),
                    ("shapeRotation".into(), PlistValue::Real(30.0)),
                    ("shapeAzimuth".into(), PlistValue::Bool(true)),
                    ("shapeScatter".into(), PlistValue::Real(0.3)),
                    ("grainDepth".into(), PlistValue::Real(0.75)),
                    ("textureScale".into(), PlistValue::Real(1.5)),
                    ("textureInverted".into(), PlistValue::Bool(true)),
                    ("plotSmoothing".into(), PlistValue::Real(0.4)),
                    ("taperStartLength".into(), PlistValue::Real(0.2)),
                    ("taperEndLength".into(), PlistValue::Real(0.35)),
                    ("dualBlendMode".into(), PlistValue::Integer(1)),
                ]),
                PlistValue::String("Ink Wash".into()),
                PlistValue::Dict(vec![("points".into(), PlistValue::Array(vec![PlistValue::Uid(4), PlistValue::Uid(5), PlistValue::Uid(6)]))]),
                PlistValue::String("{0.0, 0.0}".into()),
                PlistValue::String("{0.4, 0.7}".into()),
                PlistValue::String("{1.0, 1.0}".into()),
            ],
            1,
        );
        let sub_archive = write_bplist_objects(
            &[
                PlistValue::String("$null".into()),
                PlistValue::Dict(vec![
                    ("name".into(), PlistValue::String("Sub".into())),
                    ("paintSize".into(), PlistValue::Real(0.2)),
                    ("plotSpacing".into(), PlistValue::Real(0.25)),
                ]),
            ],
            1,
        );
        let shape = png_gray(32, 32, 220);
        let grain = png_gray(32, 32, 160);
        let sub_shape = png_gray(16, 16, 180);
        let bytes = zip_bytes(&[
            ("Brush.archive", &archive),
            ("Shape.png", &shape),
            ("Grain.png", &grain),
            ("Sub01/Brush.archive", &sub_archive),
            ("Sub01/Shape.png", &sub_shape),
        ]);

        let imp = read_procreate(&bytes, "My Pack").unwrap();
        assert_eq!(imp.presets.len(), 1);
        assert!(imp.warnings.is_empty(), "unexpected warnings: {:?}", imp.warnings);
        let p = &imp.presets[0];
        assert_eq!(p.name, "Ink Wash");
        assert_eq!(p.group, "My Pack");
        assert!((p.brush.size - 60.0).abs() < 1e-4);
        assert!((p.brush.opacity - 0.8).abs() < 1e-4);
        assert!((p.brush.spacing - 0.18).abs() < 1e-4);
        assert!(p.brush.pressure_size);
        assert!(p.brush.shape_azimuth);
        assert!((p.brush.angle - 30.0).abs() < 1e-4);
        assert!((p.brush.dynamics_tilt_size - 0.45).abs() < 1e-4);
        assert!((p.brush.taper_start_length - 0.2).abs() < 1e-4);
        assert!((p.brush.taper_end_length - 0.35).abs() < 1e-4);
        assert!(matches!(p.brush.tip, TipShape::Sampled(ref t) if t.width == 32 && t.height == 32));
        assert!(p.brush.texture.enabled && p.brush.texture.invert);
        assert!(matches!(p.brush.texture.pattern, Pattern::Tile(ref t) if t.width == 32));
        assert!(p.brush.dual_brush.enabled);
        assert!(matches!(p.brush.dual_brush.tip, TipShape::Sampled(ref t) if t.width == 16));
        assert!((p.brush.dynamics_pressure_size_curve.eval(0.4) - 0.7).abs() < 0.05);
    }

    #[test]
    fn imports_brushset_xml_and_warns_on_missing_bundled_shape() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
  <key>name</key><string>Studio Set</string>
  <key>brushes</key><array><string>UUID-1</string><string>UUID-2</string></array>
</dict></plist>"#;
        let b1 = write_bplist_objects(
            &[
                PlistValue::String("$null".into()),
                PlistValue::Dict(vec![
                    ("name".into(), PlistValue::String("Soft Pastel".into())),
                    ("paintSize".into(), PlistValue::Real(0.25)),
                    ("bundledShapePath".into(), PlistValue::String("Brush-Artery-Chalk.jpg".into())),
                    ("bundledGrainPath".into(), PlistValue::String("Acrylic-Grit.jpg".into())),
                    ("grainDepth".into(), PlistValue::Real(0.6)),
                ]),
            ],
            1,
        );
        let b2 = write_bplist_objects(
            &[
                PlistValue::String("$null".into()),
                PlistValue::Dict(vec![
                    ("name".into(), PlistValue::String("Custom Square".into())),
                    ("paintSize".into(), PlistValue::Real(0.1)),
                ]),
            ],
            1,
        );
        let shape2 = png_gray(16, 16, 255);
        let bytes = zip_bytes(&[
            ("brushset.plist", xml.as_bytes()),
            ("UUID-1/Brush.archive", &b1),
            ("UUID-2/Brush.archive", &b2),
            ("UUID-2/Shape.png", &shape2),
        ]);

        let imp = read_procreate(&bytes, "Studio Set").unwrap();
        assert_eq!(imp.presets.len(), 2);
        assert_eq!(imp.presets[0].name, "Soft Pastel");
        assert!(matches!(imp.presets[0].brush.tip, TipShape::Round));
        assert!(imp.presets[0].brush.texture.enabled);
        assert_eq!(imp.presets[1].name, "Custom Square");
        assert!(matches!(imp.presets[1].brush.tip, TipShape::Sampled(_)));
        assert!(imp.warnings.iter().any(|w| w.contains("Brush-Artery-Chalk.jpg")));
    }

    #[test]
    fn corrupt_and_hostile_archives_return_errors_without_panicking() {
        assert!(read_procreate(b"not a zip at all", "X").is_err());
        let empty_zip = zip_bytes(&[("QuickLook/Thumbnail.png", b"hi")]);
        assert!(read_procreate(&empty_zip, "X").is_err());
        let bad_plist = zip_bytes(&[("Brush.archive", b"bplist00corrupt")]);
        assert!(read_procreate(&bad_plist, "X").is_err());
    }
}

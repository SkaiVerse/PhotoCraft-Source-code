//! Photoshop's alerts when a painting tool can't paint (#11898): a press on a hidden target
//! layer, on a layer with Lock All, or with a group active (its pixels, not its mask) opens a
//! modal alert with OK instead of a status-bar message, and paints nothing. Only the tools whose
//! wording was measured have a name here ([`tool_name`]); the others keep today's status bar. The
//! Classic gradient has the same three alerts (#12017); the live Gradient keeps today's. The Remove
//! tool has one alert, without the tool's name: Lock All on a pixel layer (#12552).

use serde_json::json;

use crate::PhotocraftApp;
use crate::state::{DialogKind, Tool};

/// The alerts; `{tool}` is [`tool_name`].
pub const HIDDEN: &str = "Could not use the {tool} because the target layer is hidden.";
pub const LOCKED: &str = "Could not use the {tool} because the layer is locked.";
pub const GROUP: &str = "Could not use the {tool} because the pixels in a group cannot be edited without merging the group.";
/// The Remove tool's one measured alert, Lock All (#12552): it doesn't name the tool.
pub const REMOVE_LOCKED: &str = "Can't use the tool because the layer is locked.";

/// The colour-mode alerts (#12686); `{tool}` is [`mode_tool_name`]. The engine refuses the
/// command with the same words.
pub const INDEXED: &str = "Could not use the {tool} because it does not work with Indexed Color images (convert image to RGB Color mode to edit).";
pub const BITMAP: &str = "Could not use the {tool} because it does not work with Bitmap images (convert image to Grayscale mode to edit).";

/// The tool's name in the colour-mode alerts, for the tools whose wording was measured: the Dodge,
/// Burn, Sponge, Blur, Sharpen and Smudge tools. Kept apart from [`tool_name`]: the layer alerts
/// aren't wired for all of them yet (#12685).
fn mode_tool_name(tool: Tool) -> Option<&'static str> {
    match tool {
        Tool::Dodge => Some(tl!("dodge tool")),
        Tool::Burn => Some(tl!("burn tool")),
        Tool::Sponge => Some(tl!("sponge tool")),
        Tool::Blur => Some(tl!("blur tool")),
        Tool::Sharpen => Some(tl!("sharpen tool")),
        Tool::Smudge => Some(tl!("smudge tool")),
        _ => None,
    }
}

/// The tool's name in the alerts, for the tools whose wording was measured: the Brush, the
/// Sharpen tool (#12952), the Sponge (#13030), the Smudge tool (#12965) and the Classic gradient
/// (the live Gradient makes a fill layer and isn't measured, #12017).
pub fn tool_name(app: &PhotocraftApp, tool: Tool) -> Option<&'static str> {
    match tool {
        Tool::Brush => Some(tl!("brush tool")),
        Tool::Eraser => Some(tl!("eraser tool")),
        Tool::Sharpen => Some(tl!("sharpen tool")),
        Tool::Sponge => Some(tl!("sponge tool")),
        Tool::Smudge => Some(tl!("smudge tool")),
        Tool::Gradient if app.ui.tool_options.gradient_classic => Some(tl!("gradient tool")),
        _ => None,
    }
}

/// Human-readable lowercase tool name for locked/background layer alerts across all editing tools.
fn any_tool_name(tool: Tool) -> &'static str {
    match tool {
        Tool::Brush => tl!("brush tool"),
        Tool::Pencil => tl!("pencil tool"),
        Tool::MixerBrush => tl!("mixer brush tool"),
        Tool::ColorReplacement => tl!("color replacement tool"),
        Tool::Eraser => tl!("eraser tool"),
        Tool::BackgroundEraser => tl!("background eraser tool"),
        Tool::MagicEraser => tl!("magic eraser tool"),
        Tool::Blur => tl!("blur tool"),
        Tool::Sharpen => tl!("sharpen tool"),
        Tool::Smudge => tl!("smudge tool"),
        Tool::Dodge => tl!("dodge tool"),
        Tool::Burn => tl!("burn tool"),
        Tool::Sponge => tl!("sponge tool"),
        Tool::CloneStamp => tl!("clone stamp tool"),
        Tool::PatternStamp => tl!("pattern stamp tool"),
        Tool::HistoryBrush => tl!("history brush tool"),
        Tool::ArtHistoryBrush => tl!("art history brush tool"),
        Tool::SpotHealing => tl!("spot healing brush tool"),
        Tool::Healing => tl!("healing brush tool"),
        Tool::Remove => tl!("remove tool"),
        Tool::Patch => tl!("patch tool"),
        Tool::ContentAwareMove => tl!("content-aware move tool"),
        Tool::RedEye => tl!("red eye tool"),
        Tool::PaintBucket => tl!("paint bucket tool"),
        Tool::Gradient => tl!("gradient tool"),
        Tool::Pen | Tool::CurvaturePen => tl!("pen tool"),
        Tool::FreeformPen => tl!("freeform pen tool"),
        Tool::AddAnchorPoint | Tool::DeleteAnchorPoint | Tool::ConvertPoint | Tool::PathSelection | Tool::DirectSelection => tl!("path tool"),
        Tool::Rectangle | Tool::EllipseShape | Tool::Triangle | Tool::Polygon | Tool::Star | Tool::Line | Tool::CustomShape => tl!("shape tool"),
        Tool::Type | Tool::VerticalType | Tool::HorizontalTypeMask | Tool::VerticalTypeMask => tl!("type tool"),
        Tool::Move => tl!("move tool"),
        Tool::Artboard => tl!("artboard tool"),
        Tool::Frame => tl!("frame tool"),
        Tool::Crop | Tool::PerspectiveCrop => tl!("crop tool"),
        Tool::Slice | Tool::SliceSelect => tl!("slice tool"),
        _ => tl!("tool"),
    }
}

/// Returns a warning dialog message when `tool` cannot operate on a Background or locked layer
/// under strict Background/lock enforcement (`app.strict_background_lock`). Navigation,
/// color/measurement, and pure selection marquee/lasso/wand tools remain usable.
pub(crate) fn locked_or_background_alert(app: &PhotocraftApp, tool: Tool) -> Option<String> {
    if !app.strict_background_lock {
        return None;
    }
    if matches!(
        tool,
        Tool::Hand
            | Tool::RotateView
            | Tool::Zoom
            | Tool::Eyedropper
            | Tool::ColorSampler
            | Tool::Ruler
            | Tool::Count
            | Tool::Note
            | Tool::RectMarquee
            | Tool::EllipseMarquee
            | Tool::SingleRowMarquee
            | Tool::SingleColumnMarquee
            | Tool::Lasso
            | Tool::PolygonLasso
            | Tool::MagneticLasso
            | Tool::MagicWand
            | Tool::QuickSelection
            | Tool::ObjectSelection
    ) {
        return None;
    }
    let st = app.session.active()?;
    let id = st.active_layer?;
    let _layer = st.doc.layer(id)?;
    let locks = st.doc.effective_locks(id);
    let is_bg = st.doc.is_background(id);

    if is_bg || locks.all || locks.pixels || locks.position || locks.transparency || locks.artboard {
        return Some(crate::i18n::fmt(tl!(LOCKED), &[("tool", any_tool_name(tool))]));
    }
    None
}

/// The alert a press with `tool` gets, if any. `hidden` is the press-time hidden-target refusal
/// (`canvas::tool_input`), which this only words; the lock and the group are checked here. Lock
/// transparent pixels is no refusal, and a group's targeted mask paints. A tool with a
/// [`mode_tool_name`] gets its colour-mode alert first, in an Indexed Color or Bitmap document; the
/// Pattern Stamp only the engine's, for any pattern in an Indexed Color document.
pub fn alert(app: &PhotocraftApp, tool: Tool, hidden: bool) -> Option<String> {
    // The Pattern Stamp's colour-mode alert depends on its pattern too (#12887).
    if tool == Tool::PatternStamp {
        use photocraft_engine::retouch_cmds::{PATTERN_STAMP_INDEXED_REFUSAL, pattern_stamp_press_refusal};
        let p = crate::retouch_ui::pattern_stamp_params(app, &[]);
        return pattern_stamp_press_refusal(&app.session, &p).is_some().then(|| tl!(PATTERN_STAMP_INDEXED_REFUSAL).to_owned());
    }
    if let Some(name) = mode_tool_name(tool)
        && let Some(template) = match app.session.active()?.doc.mode {
            photocraft_doc::ColorMode::Indexed => Some(INDEXED),
            photocraft_doc::ColorMode::Bitmap => Some(BITMAP),
            _ => None,
        }
    {
        return Some(crate::i18n::fmt(tl!(template), &[("tool", name)]));
    }
    if tool == Tool::Remove {
        // Measured on a pixel layer only: a hidden target, a group, a type, shape, Smart Object or
        // fill layer keep today's behaviour.
        let st = app.session.active()?;
        let pixel_layer = st.active_layer.and_then(|id| st.doc.layer(id)).is_some_and(|l| matches!(l.content, photocraft_doc::LayerContent::Raster(_)));
        return (!hidden && pixel_layer && layer_refusal(app, tool) == Some(LOCKED)).then(|| tl!(REMOVE_LOCKED).to_string());
    }
    let name = tool_name(app, tool)?;
    let template = if hidden { HIDDEN } else { layer_refusal(app, tool)? };
    Some(crate::i18n::fmt(tl!(template), &[("tool", name)]))
}

/// [`GROUP`] or [`LOCKED`] when `tool` can't paint the active layer's pixels.
fn layer_refusal(app: &PhotocraftApp, tool: Tool) -> Option<&'static str> {
    let st = app.session.active()?;
    let layer = st.active_layer.and_then(|id| st.doc.layer(id))?;
    if crate::canvas::paint_target(app) != json!("pixels") {
        None
    } else if layer.is_group() {
        Some(GROUP)
    } else if locked(st.doc.effective_locks(layer.id), tool)
        || (tool == Tool::Eraser
            && (st.doc.is_background(layer.id)
                || st.doc.effective_locks(layer.id).all
                || st.doc.effective_locks(layer.id).pixels
                || st.doc.effective_locks(layer.id).transparency))
    {
        Some(LOCKED)
    } else {
        None
    }
}

/// Whether `locks` refuse `tool`: Lock All, and for the Sharpen, Sponge and Smudge tools Lock Image
/// Pixels as well (measured for them; the Brush keeps today's behaviour there).
fn locked(locks: photocraft_doc::Locks, tool: Tool) -> bool {
    locks.all || (matches!(tool, Tool::Sharpen | Tool::Sponge | Tool::Smudge) && locks.pixels)
}

/// Opens the alert: ordinary dialog state, listed by `ui.inspect` and closed by `ui.dialog.confirm`.
pub fn prompt(app: &mut PhotocraftApp, message: &str) {
    app.ui.open_dialog(DialogKind::Error, json!({ "message": message }).as_object().cloned().unwrap_or_default());
}

#[cfg(test)]
#[path = "../brush_refusal_alert_tests.rs"]
pub(super) mod brush_refusal_alert_tests;

#[cfg(test)]
#[path = "../sharpen_refusal_alert_tests.rs"]
mod sharpen_refusal_alert_tests;

#[cfg(test)]
#[path = "../sponge_refusal_alert_tests.rs"]
mod sponge_refusal_alert_tests;

#[cfg(test)]
#[path = "../retouch_mode_refusal_alert_tests.rs"]
mod retouch_mode_refusal_alert_tests;

#[cfg(test)]
#[path = "../smudge_refusal_alert_tests.rs"]
mod smudge_refusal_alert_tests;

#[cfg(test)]
#[path = "../bucket_bitmap_alert_tests.rs"]
mod bucket_bitmap_alert_tests;

#[cfg(test)]
#[path = "remove_locked_alert_tests.rs"]
mod remove_locked_alert_tests;

#[cfg(test)]
#[path = "remove_hidden_layer_tests.rs"]
mod remove_hidden_layer_tests;

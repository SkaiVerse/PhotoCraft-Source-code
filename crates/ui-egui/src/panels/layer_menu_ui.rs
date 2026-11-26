//! Layers panel right-click menu in Photoshop's order. Items are command ids from the menu catalog;
//! unavailable ones are greyed using the same enablement as the main menus. Items marked as menu
//! invocations (`Value::Null` params) go through `menus::invoke`, so dialogs open like in the menu bar.

use photocraft_doc::{LabelColor, Layer, LayerContent};
use serde_json::{Value, json};

/// One entry: label, command id. `None` = separator.
pub type Entry = Option<(&'static str, &'static str)>;

/// English action key; each menu translates it using its existing display context.
pub(crate) fn mask_toggle_label(enabled: bool) -> &'static str {
    if enabled { "Disable Layer Mask" } else { "Enable Layer Mask" }
}

/// The command behind "Add Layer Mask" (the panel button and this menu). Like Photoshop, an active
/// selection becomes the mask; ⌥ inverts it (Hide Selection, or Hide All without a selection).
pub fn add_mask_command(has_selection: bool, alt: bool) -> &'static str {
    match (has_selection, alt) {
        (true, false) => "layer.layerMask.revealSelection",
        (true, true) => "layer.layerMask.hideSelection",
        (false, false) => "layer.layerMask.revealAll",
        (false, true) => "layer.layerMask.hideAll",
    }
}

/// What the Layers panel "Add a mask" button does for `layer` (#2075). Like Photoshop it never
/// replaces a mask: a layer without a layer mask gets one ([`add_mask_command`]); a layer that
/// already has one gets a vector mask instead (⌥: Hide All); a layer with both (or a shape layer,
/// whose path is already its vector mask, with a layer mask) gets nothing (`None`).
pub fn mask_button_command(layer: Option<&Layer>, has_selection: bool, alt: bool) -> Option<&'static str> {
    let l = layer?;
    if l.mask.is_none() {
        Some(add_mask_command(has_selection, alt))
    } else if l.vector_mask.is_none() && !matches!(l.content, LayerContent::Shape(_)) {
        Some(if alt { "layer.vectorMask.hideAll" } else { "layer.vectorMask.revealAll" })
    } else {
        None
    }
}

/// Commands that act on the active layer only and have no sensible meaning for a selection: Blending
/// Options (one dialog), Copy Layer Style (copy from which layer?) and the per-layer mask items.
/// With several layers selected they would hit whichever layer is active, not the one that was
/// right-clicked, so the menu greys them. Adding a mask, Paste/Clear Layer Style, Rasterize and
/// Export As (one file per layer, #3629) run on every selected layer instead.
pub fn single_layer_only(id: &str) -> bool {
    let adds_mask = matches!(id, "layer.layerMask.revealAll" | "layer.layerMask.hideAll" | "layer.layerMask.revealSelection" | "layer.layerMask.hideSelection");
    matches!(id, "layer.layerStyle.blendingOptions" | "layer.layerStyle.copyLayerStyle") || (id.starts_with("layer.layerMask.") && !adds_mask)
}

/// The context menu entries for a layer (Photoshop 2026 order, trimmed to the layer kind).
pub fn entries(l: &Layer, multi: bool, has_selection: bool) -> Vec<Entry> {
    let mut v: Vec<Entry> = vec![Some((tl!("Blending Options…"), "layer.layerStyle.blendingOptions"))];
    v.push(None);
    v.push(Some((if multi { tl!("Duplicate Layers…") } else { tl!("Duplicate Layer…") }, "layer.duplicate")));
    v.push(Some((if multi { tl!("Delete Layers") } else { tl!("Delete Layer") }, "layer.delete")));
    // Groups the clicked layer, or the whole selection when several layers are selected, after
    // Photoshop's New Group dialog.
    v.push(Some((tl!("Group from Layers…"), "layer.new.groupFromLayers")));
    v.push(None);
    v.push(Some((tl!("Quick Export as PNG"), "layer.quickExportAsPng")));
    v.push(Some((tl!("Export As…"), "layer.exportAs")));
    v.push(None);
    v.push(Some((tl!("Artboard from Layers…"), "layer.new.artboardFromLayers")));
    v.push(Some((tl!("Frame from Layers…"), "layer.new.frameFromLayers")));
    if !multi && matches!(l.content, LayerContent::Shape(_) | LayerContent::Text(_)) {
        v.push(Some((tl!("Convert to Frame…"), "layer.frame.convert")));
    }
    v.push(None);
    v.push(Some((tl!("Convert to Smart Object"), "layer.smartObjects.convertToSmartObject")));
    if multi {
        // One item for the whole selection, whatever the clicked layer is: Rasterize acts on every
        // selected layer. Edit Contents and Convert to Layers work on one layer, so they are not
        // offered here.
        v.push(Some(("Rasterize Layers", "layer.rasterize.layer")));
    } else {
        match &l.content {
            LayerContent::Text(_) => {
                v.push(Some(("Rasterize Type", "layer.rasterize.type")));
                v.push(Some((tl!("Create Work Path"), "type.createWorkPath")));
                v.push(Some((tl!("Convert to Shape"), "type.convertToShape")));
            }
            LayerContent::Shape(_) => v.push(Some(("Rasterize Layer", "layer.rasterize.shape"))),
            LayerContent::Smart(_) => {
                v.push(Some((tl!("Edit Contents"), "layer.smartObjects.editContents")));
                v.push(Some((tl!("Reset Transform"), "layer.smartObjects.resetTransform")));
                v.push(Some((tl!("Replace Contents…"), "layer.smartObjects.replaceContents")));
                v.push(Some((tl!("Relink to File…"), "layer.smartObjects.relinkToFile")));
                v.push(Some((tl!("Convert to Layers"), "layer.smartObjects.convertToLayers")));
                v.push(Some(("Rasterize Layer", "layer.rasterize.smartObject")));
            }
            LayerContent::Fill(_) => v.push(Some(("Rasterize Layer", "layer.rasterize.fillContent"))),
            _ => v.push(Some(("Rasterize Layer", "layer.rasterize.layer"))),
        }
    }
    // Photoshop offers it under Rasterize Layer for a layer with a style (it bakes the effects).
    if multi || !l.effects.items.is_empty() {
        v.push(Some(("Rasterize Layer Style", "layer.rasterize.layerStyle")));
    }
    v.push(None);
    // A selection has no single mask to toggle, apply or delete, so its menu offers Add Layer Mask only.
    if let Some(mask) = l.mask.as_ref().filter(|_| !multi) {
        v.push(Some((tl!(mask_toggle_label(mask.enabled)), "layer.layerMask.enabled")));
        v.push(Some((tl!("Apply Layer Mask"), "layer.layerMask.apply")));
        v.push(Some((tl!("Delete Layer Mask"), "layer.layerMask.delete")));
    } else {
        v.push(Some((tl!("Add Layer Mask"), add_mask_command(has_selection, false))));
    }
    v.push(Some((
        if l.clipped { tl!("Release Clipping Mask") } else { tl!("Create Clipping Mask") },
        if l.clipped { "layer.releaseClippingMask" } else { "layer.createClippingMask" },
    )));
    v.push(None);
    v.push(Some((tl!("Link Layers"), "layer.linkLayers")));
    v.push(Some((tl!("Select Linked Layers"), "layer.selectLinkedLayers")));
    v.push(None);
    v.push(Some((tl!("Copy Layer Style"), "layer.layerStyle.copyLayerStyle")));
    v.push(Some((tl!("Paste Layer Style"), "layer.layerStyle.pasteLayerStyle")));
    v.push(Some((tl!("Clear Layer Style"), "layer.layerStyle.clear")));
    v.push(None);
    if multi {
        v.push(Some((tl!("Merge Layers"), "layer.mergeLayers")));
    } else if matches!(l.content, LayerContent::Group(_)) {
        // Photoshop's Merge Group: a group merges itself, not into the layer below (#3832).
        v.push(Some((tl!("Merge Group"), "layer.mergeGroup")));
    } else {
        v.push(Some((tl!("Merge Down"), "layer.mergeDown")));
    }
    v.push(Some((tl!("Merge Visible"), "layer.mergeVisible")));
    v.push(Some((tl!("Flatten Image"), "layer.flattenImage")));
    v
}

/// The layer style menu of an effect row ("Effects" or one effect) under a layer (#2846), in
/// Photoshop's order. It acts on that layer, like Layer ▸ Layer Style.
pub fn effects_entries() -> Vec<Entry> {
    vec![
        Some((tl!("Blending Options…"), "layer.layerStyle.blendingOptions")),
        None,
        Some((tl!("Copy Layer Style"), "layer.layerStyle.copyLayerStyle")),
        Some((tl!("Paste Layer Style"), "layer.layerStyle.pasteLayerStyle")),
        Some((tl!("Clear Layer Style"), "layer.layerStyle.clear")),
        None,
        Some((tl!("Global Light…"), "layer.layerStyle.globalLight")),
        Some((tl!("Create Layer"), "layer.layerStyle.createLayer")),
        Some((tl!("Hide All Effects"), "layer.layerStyle.hideAllEffects")),
        Some((tl!("Scale Effects…"), "layer.layerStyle.scaleEffects")),
    ]
}

/// Render the menu. Pushes `(command, params)` actions; `Value::Null` params mean "invoke like the
/// menu item" (opens the command's dialog when it has one).
pub fn show(app: &crate::PhotocraftApp, ui: &mut egui::Ui, l: &Layer, on_set: bool, actions: &mut Vec<(String, Value)>) -> bool {
    crate::widgets::menu_scroll(ui, |ui| {
        ui.set_min_width(220.0);
        let mut rename = false;
        let has_selection = app.session.active().is_some_and(|s| s.doc.selection.is_some());
        items(app, ui, l, on_set, entries(l, on_set, has_selection), actions);
        ui.separator();
        if ui.button(tl!("Rename Layer…")).clicked() {
            rename = true;
            ui.close();
        }
        color_menu(app, ui, l, on_set, actions);
        rename
    })
}

/// The menu of `l`'s layer-mask thumbnail (#4575), in Photoshop's order. It acts on `l`, which it
/// selects first like the row menu.
pub fn show_mask(app: &crate::PhotocraftApp, ui: &mut egui::Ui, l: &Layer, actions: &mut Vec<(String, Value)>) {
    let Some(mask) = &l.mask else { return };
    crate::widgets::menu_scroll(ui, |ui| {
        ui.set_min_width(220.0);
        let edits = vec![
            Some((tl!(mask_toggle_label(mask.enabled)), "layer.layerMask.enabled")),
            Some((tl!("Delete Layer Mask"), "layer.layerMask.delete")),
            Some((tl!("Apply Layer Mask"), "layer.layerMask.apply")),
        ];
        items(app, ui, l, false, edits, actions);
        ui.separator();
        // ⌘⇧-, ⌘⌥- and ⌘⇧⌥-clicking the thumbnail, as menu items.
        let loads =
            [(tl!("Add Mask to Selection"), "add"), (tl!("Subtract Mask from Selection"), "subtract"), (tl!("Intersect Mask with Selection"), "intersect")];
        for (label, operation) in loads {
            if ui.button(label).clicked() {
                actions.push(("layer.select".into(), json!({"layer": l.id.0})));
                actions.push(("select.loadSelection".into(), json!({"channel": "mask", "layer": l.id.0, "operation": operation})));
                ui.close();
            }
        }
        ui.separator();
        if ui.add_enabled(crate::menus::is_enabled(app, "select.selectAndMask"), egui::Button::new(tl!("Select and Mask…"))).clicked() {
            // Like double-clicking the thumbnail with that preference on: it refines the mask.
            actions.push(("layer.select".into(), json!({"layer": l.id.0})));
            actions.push(("ui.maskTarget".into(), json!(true)));
            actions.push(("select.selectAndMask".into(), Value::Null));
            ui.close();
        }
        // Layer Mask Display Options: the rubylith's colour and opacity (`channel.options`).
        if ui.button(tl!("Mask Options…")).clicked() {
            actions.push(("layer.select".into(), json!({"layer": l.id.0})));
            actions.push(("ui.maskTarget".into(), json!(true)));
            actions.push(("channel.options".into(), Value::Null));
            ui.close();
        }
    });
}

/// Render the layer style menu of `l`'s effect rows ([`effects_entries`]); the actions select `l`
/// first when it isn't the active layer.
pub fn show_effects(app: &crate::PhotocraftApp, ui: &mut egui::Ui, l: &Layer, actions: &mut Vec<(String, Value)>) {
    crate::widgets::menu_scroll(ui, |ui| {
        ui.set_min_width(200.0);
        items(app, ui, l, false, effects_entries(), actions);
    });
}

/// The command items of a layer menu, separators collapsed.
fn items(app: &crate::PhotocraftApp, ui: &mut egui::Ui, l: &Layer, on_set: bool, entries: Vec<Entry>, actions: &mut Vec<(String, Value)>) {
    let mut last_sep = true;
    for e in entries {
        match e {
            None => {
                if !last_sep {
                    ui.separator();
                }
                last_sep = true;
            }
            Some((label, id)) => {
                // Skip commands this build doesn't have rather than showing dead items.
                if photocraft_engine::commands::find(id).is_none() && !crate::menu_catalog::CATALOG.iter().any(|m| m.3 == id) {
                    continue;
                }
                last_sep = false;
                // Enablement is exact for the active layer (or the selection); another row is
                // selected first when clicked, so its items stay available.
                let is_active = app.session.active().is_some_and(|s| s.active_layer == Some(l.id));
                let enabled = !(on_set && single_layer_only(id)) && if on_set || is_active { crate::menus::is_enabled(app, id) } else { true };
                if ui.add_enabled(enabled, egui::Button::new(tl!(&label))).clicked() {
                    if !on_set {
                        actions.push(("layer.select".into(), json!({"layer": l.id.0})));
                    }
                    let params = if id == "layer.delete" {
                        let alt = ui.input(|i| i.modifiers.alt);
                        if alt {
                            json!({})
                        } else {
                            json!({"__trash": true, "layer": l.id.0})
                        }
                    } else {
                        Value::Null
                    };
                    actions.push((id.into(), params));
                    ui.close();
                }
            }
        }
    }
}

/// Render the Affinity-style touch layer popover menu matching the clean dark card layout
/// (`Add to Selection`, `Duplicate`, `Delete`, `Hide`, `Solo`, `Lock`, `Rename Layer...`,
/// `Create Clipping Mask`, `Release`, `Merge ▸`, `Rasterise ▸`, `Expand Selection`,
/// `Collapse Selection`, `Collapse All Parents`, and the `Tag` 8-circle colour row).
/// Returns `(rename_requested, action_dispatched)`.
pub(crate) fn show_affinity_popover(
    app: &crate::PhotocraftApp,
    ui: &mut egui::Ui,
    l: &Layer,
    multi: bool,
    actions: &mut Vec<(String, Value)>,
) -> bool {
    use egui::{Color32, Pos2, Rect, Sense, Stroke, pos2, vec2};

    let Some(st) = app.session.active() else { return false };
    let doc = &st.doc;
    let ctx = ui.ctx().clone();
    let is_bg = doc.is_background(l.id);
    let is_locked = is_bg || l.locks.all;
    let solo_active = st.show_only.as_ref().is_some_and(|(id, _)| *id == l.id);
    let can_clip_create = !l.clipped && doc.path_of(l.id).and_then(|p| p.last().copied()).is_some_and(|idx| idx > 0);
    let can_clip_release = l.clipped;

    let selected = st.selected_layers();
    let check_ids: Vec<photocraft_doc::LayerId> = if multi && selected.contains(&l.id) { selected.clone() } else { vec![l.id] };
    let can_expand_sel = check_ids
        .iter()
        .any(|id| doc.layer(*id).is_some_and(|x| matches!(&x.content, LayerContent::Group(g) if !g.expanded)));
    let can_collapse_sel = check_ids
        .iter()
        .any(|id| doc.layer(*id).is_some_and(|x| matches!(&x.content, LayerContent::Group(g) if g.expanded)));
    let any_group_expanded = doc.walk().iter().any(|(_, _, g)| matches!(&g.content, LayerContent::Group(gr) if gr.expanded));
    let can_merge_down = if multi {
        selected.len() > 1
    } else if l.is_group() {
        true
    } else {
        doc.path_of(l.id).and_then(|p| p.last().copied()).is_some_and(|idx| idx > 0)
    };
    let can_delete = !(doc.layers.len() == 1 && is_bg);

    let sub_key = egui::Id::unique(("affinity-layer-menu-sub", l.id.0));
    // 0 = none, 1 = Merge expanded, 2 = Rasterise expanded
    let mut open_sub = ctx.data(|d| d.get_temp::<u8>(sub_key)).unwrap_or(0);

    let text_enabled = Color32::from_rgb(232, 232, 235);
    let text_disabled = Color32::from_rgb(104, 104, 110);
    let divider_color = Color32::from_rgba_unmultiplied(255, 255, 255, 26);
    let hover_bg = Color32::from_rgba_unmultiplied(255, 255, 255, 24);
    let row_h = 28.0_f32;
    let w = ui.available_width().max(220.0);

    let menu_row = |ui: &mut egui::Ui, label: &str, enabled: bool, submenu_arrow: bool, active: bool| -> bool {
        let (rect, resp) = ui.allocate_exact_size(vec2(w, row_h), if enabled { Sense::click() } else { Sense::hover() });
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, enabled, label));
        if enabled && (resp.hovered() || active) {
            ui.painter().rect_filled(rect, 5.0, hover_bg);
        }
        let col = if enabled { text_enabled } else { text_disabled };
        let font = egui::FontId::proportional(14.0);
        let galley = ui.painter().layout_no_wrap(label.to_string(), font, col);
        let pos = pos2(rect.left() + 12.0, rect.center().y - galley.size().y * 0.5);
        ui.painter().galley(pos, galley, col);
        if submenu_arrow {
            let cx = rect.right() - 14.0;
            let cy = rect.center().y;
            let arrow_col = Color32::from_rgb(138, 138, 144);
            let pts = if active {
                vec![pos2(cx - 4.0, cy - 2.5), pos2(cx + 3.0, cy - 2.5), pos2(cx - 0.5, cy + 3.0)]
            } else {
                vec![pos2(cx - 2.5, cy - 4.0), pos2(cx + 3.0, cy), pos2(cx - 2.5, cy + 4.0)]
            };
            ui.painter().add(egui::Shape::convex_polygon(pts, arrow_col, Stroke::NONE));
        }
        enabled && resp.clicked()
    };

    let sep = |ui: &mut egui::Ui| {
        let (r, _) = ui.allocate_exact_size(vec2(w, 7.0), Sense::hover());
        let y = r.center().y;
        ui.painter().line_segment([pos2(r.left() + 2.0, y), pos2(r.right() - 2.0, y)], Stroke::new(1.0, divider_color));
    };

    let mut rename_clicked = false;
    let has_selection = st.doc.selection.is_some();
    let is_active = st.active_layer == Some(l.id);

    let dispatch_pc = |ui: &mut egui::Ui, actions: &mut Vec<(String, Value)>, label: &str, id: &str| {
        if photocraft_engine::commands::find(id).is_none() && !crate::menu_catalog::CATALOG.iter().any(|m| m.3 == id) {
            return;
        }
        let enabled = !(multi && single_layer_only(id))
            && (id != "layer.delete" || can_delete)
            && if multi || is_active { crate::menus::is_enabled(app, id) } else { true };
        if menu_row(ui, label, enabled, false, false) {
            if !multi {
                actions.push(("layer.select".into(), json!({"layer": l.id.0})));
            }
            let params = if id == "layer.delete" {
                let alt = ui.input(|i| i.modifiers.alt);
                if alt {
                    json!({})
                } else {
                    json!({"__trash": true, "layer": l.id.0})
                }
            } else {
                Value::Null
            };
            actions.push((id.into(), params));
        }
    };

    crate::widgets::menu_scroll(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 0.0;

        if menu_row(ui, tl!("Add to Selection"), true, false, false) {
            actions.push(("layer.select".into(), json!({"layer": l.id.0, "mode": "add"})));
            ctx.data_mut(|d| {
                let list = d.get_temp_mut_or_default::<Vec<u64>>(crate::layer_row_ui::swipe_select_key());
                if !list.contains(&l.id.0) {
                    list.push(l.id.0);
                }
            });
        }
        dispatch_pc(ui, actions, tl!("Blending Options…"), "layer.layerStyle.blendingOptions");
        dispatch_pc(ui, actions, if multi { tl!("Duplicate Layers…") } else { tl!("Duplicate Layer…") }, "layer.duplicate");
        dispatch_pc(ui, actions, if multi { tl!("Delete Layers") } else { tl!("Delete Layer") }, "layer.delete");
        dispatch_pc(ui, actions, tl!("Group from Layers…"), "layer.new.groupFromLayers");

        sep(ui);

        if menu_row(ui, if l.visible { tl!("Hide") } else { tl!("Show") }, true, false, false) {
            actions.push(("layer.setProps".into(), json!({"layer": l.id.0, "visible": !l.visible})));
        }
        if menu_row(ui, tl!("Solo"), true, false, solo_active) {
            actions.push(("layer.showOnly".into(), json!({"layer": l.id.0})));
        }
        if menu_row(ui, if is_locked { tl!("Unlock") } else { tl!("Lock") }, true, false, false) {
            if is_bg {
                actions.push(("layer.new.layerFromBackground".into(), json!({})));
            } else {
                actions.push(("layer.lockLayers".into(), json!({"layer": l.id.0, "all": !l.locks.all})));
            }
        }
        if menu_row(ui, tl!("Rename Layer..."), true, false, false) {
            rename_clicked = true;
        }

        sep(ui);

        dispatch_pc(ui, actions, tl!("Quick Export as PNG"), "layer.quickExportAsPng");
        dispatch_pc(ui, actions, tl!("Export As…"), "layer.exportAs");
        dispatch_pc(ui, actions, tl!("Artboard from Layers…"), "layer.new.artboardFromLayers");
        dispatch_pc(ui, actions, tl!("Frame from Layers…"), "layer.new.frameFromLayers");
        if !multi && matches!(l.content, LayerContent::Shape(_) | LayerContent::Text(_)) {
            dispatch_pc(ui, actions, tl!("Convert to Frame…"), "layer.frame.convert");
        }

        sep(ui);

        dispatch_pc(ui, actions, tl!("Convert to Smart Object"), "layer.smartObjects.convertToSmartObject");
        if multi {
            dispatch_pc(ui, actions, tl!("Rasterize Layers"), "layer.rasterize.layer");
        } else {
            match &l.content {
                LayerContent::Text(_) => {
                    dispatch_pc(ui, actions, tl!("Rasterize Type"), "layer.rasterize.type");
                    dispatch_pc(ui, actions, tl!("Create Work Path"), "type.createWorkPath");
                    dispatch_pc(ui, actions, tl!("Convert to Shape"), "type.convertToShape");
                }
                LayerContent::Shape(_) => dispatch_pc(ui, actions, tl!("Rasterize Layer"), "layer.rasterize.shape"),
                LayerContent::Smart(_) => {
                    dispatch_pc(ui, actions, tl!("Edit Contents"), "layer.smartObjects.editContents");
                    dispatch_pc(ui, actions, tl!("Reset Transform"), "layer.smartObjects.resetTransform");
                    dispatch_pc(ui, actions, tl!("Replace Contents…"), "layer.smartObjects.replaceContents");
                    dispatch_pc(ui, actions, tl!("Relink to File…"), "layer.smartObjects.relinkToFile");
                    dispatch_pc(ui, actions, tl!("Convert to Layers"), "layer.smartObjects.convertToLayers");
                    dispatch_pc(ui, actions, tl!("Rasterize Layer"), "layer.rasterize.smartObject");
                }
                LayerContent::Fill(_) => dispatch_pc(ui, actions, tl!("Rasterize Layer"), "layer.rasterize.fillContent"),
                _ => dispatch_pc(ui, actions, tl!("Rasterize Layer"), "layer.rasterize.layer"),
            }
        }
        if multi || !l.effects.items.is_empty() {
            dispatch_pc(ui, actions, tl!("Rasterize Layer Style"), "layer.rasterize.layerStyle");
        }

        if menu_row(ui, tl!("Rasterise"), true, true, open_sub == 2) {
            open_sub = if open_sub == 2 { 0 } else { 2 };
            ctx.data_mut(|d| d.insert_temp(sub_key, open_sub));
        }
        if open_sub == 2 {
            ui.indent("affinity-rasterise-sub", |ui| {
                dispatch_pc(ui, actions, tl!("Rasterise Layer"), "layer.rasterize.layer");
                if !l.effects.items.is_empty() || multi {
                    dispatch_pc(ui, actions, tl!("Rasterise Layer Style"), "layer.rasterize.layerStyle");
                }
                if menu_row(ui, tl!("Rasterise All"), true, false, false) {
                    actions.push(("layer.rasterize.allLayers".into(), json!({})));
                }
            });
        }

        sep(ui);

        if let Some(mask) = l.mask.as_ref().filter(|_| !multi) {
            dispatch_pc(ui, actions, tl!(mask_toggle_label(mask.enabled)), "layer.layerMask.enabled");
            dispatch_pc(ui, actions, tl!("Apply Layer Mask"), "layer.layerMask.apply");
            dispatch_pc(ui, actions, tl!("Delete Layer Mask"), "layer.layerMask.delete");
        } else {
            dispatch_pc(ui, actions, tl!("Add Layer Mask"), add_mask_command(has_selection, false));
        }
        if l.clipped {
            if menu_row(ui, tl!("Release Clipping Mask"), can_clip_release, false, false) {
                if !multi {
                    actions.push(("layer.select".into(), json!({"layer": l.id.0})));
                }
                actions.push(("layer.releaseClippingMask".into(), Value::Null));
            }
        } else if menu_row(ui, tl!("Create Clipping Mask"), can_clip_create || crate::menus::is_enabled(app, "layer.createClippingMask"), false, false) {
            if !multi {
                actions.push(("layer.select".into(), json!({"layer": l.id.0})));
            }
            actions.push(("layer.createClippingMask".into(), Value::Null));
        }

        sep(ui);

        dispatch_pc(ui, actions, tl!("Link Layers"), "layer.linkLayers");
        dispatch_pc(ui, actions, tl!("Select Linked Layers"), "layer.selectLinkedLayers");

        sep(ui);

        dispatch_pc(ui, actions, tl!("Copy Layer Style"), "layer.layerStyle.copyLayerStyle");
        dispatch_pc(ui, actions, tl!("Paste Layer Style"), "layer.layerStyle.pasteLayerStyle");
        dispatch_pc(ui, actions, tl!("Clear Layer Style"), "layer.layerStyle.clear");

        sep(ui);

        if multi {
            dispatch_pc(ui, actions, tl!("Merge Layers"), "layer.mergeLayers");
        } else if matches!(l.content, LayerContent::Group(_)) {
            dispatch_pc(ui, actions, tl!("Merge Group"), "layer.mergeGroup");
        } else {
            dispatch_pc(ui, actions, tl!("Merge Down"), "layer.mergeDown");
        }
        dispatch_pc(ui, actions, tl!("Merge Visible"), "layer.mergeVisible");
        dispatch_pc(ui, actions, tl!("Flatten Image"), "layer.flattenImage");

        if menu_row(ui, tl!("Merge"), true, true, open_sub == 1) {
            open_sub = if open_sub == 1 { 0 } else { 1 };
            ctx.data_mut(|d| d.insert_temp(sub_key, open_sub));
        }
        if open_sub == 1 {
            ui.indent("affinity-merge-sub", |ui| {
                if multi {
                    if menu_row(ui, tl!("Merge Selected"), can_merge_down, false, false) {
                        actions.push(("layer.mergeLayers".into(), json!({})));
                    }
                } else if l.is_group() {
                    if menu_row(ui, tl!("Merge Group"), true, false, false) {
                        actions.push(("layer.mergeGroup".into(), json!({"layer": l.id.0})));
                    }
                } else if menu_row(ui, tl!("Merge Down"), can_merge_down, false, false) {
                    actions.push(("layer.mergeDown".into(), json!({"layer": l.id.0})));
                }
                if menu_row(ui, tl!("Merge Visible"), true, false, false) {
                    actions.push(("layer.mergeVisible".into(), json!({})));
                }
                if menu_row(ui, tl!("Flatten Image"), true, false, false) {
                    actions.push(("layer.flattenImage".into(), json!({})));
                }
            });
        }

        sep(ui);

        if menu_row(ui, tl!("Expand Selection"), can_expand_sel, false, false) {
            for &id in &check_ids {
                actions.push(("layer.setExpanded".into(), json!({"layer": id.0, "expanded": true})));
            }
        }
        if menu_row(ui, tl!("Collapse Selection"), can_collapse_sel, false, false) {
            for &id in &check_ids {
                actions.push(("layer.setExpanded".into(), json!({"layer": id.0, "expanded": false})));
            }
        }
        if menu_row(ui, tl!("Collapse All Parents"), any_group_expanded, false, false) {
            actions.push(("layer.setExpanded".into(), json!({"all": true, "expanded": false})));
        }

        sep(ui);

        // Affinity Tag header + 8 circular colour swatches
        let (tag_hdr, _) = ui.allocate_exact_size(vec2(w, 20.0), Sense::hover());
        let tag_galley = ui.painter().layout_no_wrap(tl!("Tag").into(), egui::FontId::proportional(12.0), Color32::from_rgb(200, 200, 206));
        ui.painter().galley(pos2(tag_hdr.left() + 12.0, tag_hdr.center().y - tag_galley.size().y * 0.5), tag_galley, Color32::from_rgb(200, 200, 206));

        let (tag_row, _) = ui.allocate_exact_size(vec2(w, 34.0), Sense::hover());
        let tags: [(LabelColor, Color32); 8] = [
            (LabelColor::None, Color32::WHITE),
            (LabelColor::Red, Color32::from_rgb(255, 69, 58)),
            (LabelColor::Orange, Color32::from_rgb(255, 159, 10)),
            (LabelColor::Yellow, Color32::from_rgb(255, 214, 10)),
            (LabelColor::Green, Color32::from_rgb(48, 209, 88)),
            (LabelColor::Blue, Color32::from_rgb(10, 132, 255)),
            (LabelColor::Violet, Color32::from_rgb(191, 90, 242)),
            (LabelColor::Gray, Color32::from_rgb(142, 142, 147)),
        ];
        let pad_x = 12.0_f32;
        let span_w = (tag_row.width() - pad_x * 2.0).max(160.0);
        let step_x = span_w / 8.0;
        let current_tag = common_color(app, l, multi);
        for (idx, (color, fill)) in tags.into_iter().enumerate() {
            let cx = tag_row.left() + pad_x + step_x * (idx as f32 + 0.5);
            let cy = tag_row.center().y;
            let hit_rect = Rect::from_center_size(Pos2::new(cx, cy), vec2(26.0, 26.0));
            let resp = ui.interact(hit_rect, egui::Id::unique(("affinity-tag-dot", l.id.0, idx)), Sense::click());
            let selected = current_tag == Some(color);
            let radius = if color == LabelColor::None { 10.5 } else { 9.5 };
            if selected {
                ui.painter().circle_stroke(pos2(cx, cy), radius + 2.5, Stroke::new(1.5, Color32::WHITE));
            }
            if color == LabelColor::None {
                ui.painter().circle_filled(pos2(cx, cy), radius, Color32::WHITE);
                ui.painter().circle_stroke(pos2(cx, cy), radius, Stroke::new(1.0, Color32::from_rgb(180, 180, 185)));
                let s = radius * 0.65;
                ui.painter().line_segment([pos2(cx - s, cy + s), pos2(cx + s, cy - s)], Stroke::new(1.8, Color32::from_rgb(10, 132, 255)));
            } else {
                ui.painter().circle_filled(pos2(cx, cy), radius, fill);
                if resp.hovered() {
                    ui.painter().circle_stroke(pos2(cx, cy), radius + 1.0, Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 160)));
                }
            }
            if resp.clicked() {
                let params = if multi { json!({"color": color.id()}) } else { json!({"layer": l.id.0, "color": color.id()}) };
                actions.push(("layer.setLabelColor".into(), params));
            }
        }
    });

    if !actions.is_empty() || rename_clicked {
        ctx.data_mut(|d| d.remove::<u8>(sub_key));
    }
    rename_clicked
}

pub(crate) fn color_name(color: LabelColor) -> &'static str {
    crate::i18n::tr_ctx(crate::i18n::current(), "layerLabel", color.label())
}

fn common_color(app: &crate::PhotocraftApp, l: &Layer, on_set: bool) -> Option<LabelColor> {
    if !on_set {
        return Some(l.label);
    }
    let st = app.session.active()?;
    let ids = st.selected_layers();
    let mut labels = ids.iter().filter_map(|id| st.doc.layer(*id).map(|l| l.label));
    let first = labels.next()?;
    labels.all(|c| c == first).then_some(first)
}

fn color_menu(app: &crate::PhotocraftApp, ui: &mut egui::Ui, l: &Layer, on_set: bool, actions: &mut Vec<(String, Value)>) {
    ui.menu_button(tl!("Color"), |ui| {
        color_choices(app, ui, l, on_set, actions);
    });
}

/// The eye column opens the same color choices directly, without the row's full context menu.
pub(crate) fn color_choices(app: &crate::PhotocraftApp, ui: &mut egui::Ui, l: &Layer, on_set: bool, actions: &mut Vec<(String, Value)>) {
    let current = common_color(app, l, on_set);
    crate::widgets::menu_scroll(ui, |ui| {
        ui.set_min_width(170.0);
        for color in LabelColor::ALL {
            if color == LabelColor::Red {
                ui.separator();
            }
            if color_button(ui, color, current == Some(color)).clicked() {
                let params = if on_set {
                    json!({"color": color.id()})
                } else {
                    actions.push(("layer.select".into(), json!({"layer": l.id.0})));
                    json!({"layer": l.id.0, "color": color.id()})
                };
                actions.push(("layer.setLabelColor".into(), params));
                ui.close();
            }
        }
    });
}

fn color_button(ui: &mut egui::Ui, color: LabelColor, checked: bool) -> egui::Response {
    use egui::{Atom, Rect, Stroke, StrokeKind, vec2};
    let t = crate::theme::Tokens::get(ui.ctx());
    let check_id = egui::IdSalt::new(("label-check", color.id()));
    let swatch_id = egui::IdSalt::new(("label-swatch", color.id()));
    let name = color_name(color);
    let button = egui::Button::new((Atom::custom(check_id, vec2(14.0, 16.0)), Atom::custom(swatch_id, vec2(16.0, 16.0)), name, Atom::grow())).atom_ui(ui);
    if checked && let Some(rect) = button.rect(check_id) {
        crate::icons::paint(ui, rect, "check", 14.0, t.icon);
    }
    if let Some(rect) = button.rect(swatch_id) {
        let rect = Rect::from_center_size(rect.center(), vec2(16.0, 16.0));
        let p = ui.painter();
        if let Some((swatch, _)) = t.layer_label_colors(color) {
            p.rect_filled(rect, t.radius_sm, swatch);
        } else {
            let stroke = Stroke::new(1.0, t.text_dim);
            p.rect_stroke(rect, t.radius_sm, stroke, StrokeKind::Inside);
            let r = rect.shrink(4.0);
            p.line_segment([r.left_top(), r.right_bottom()], stroke);
            p.line_segment([r.right_top(), r.left_bottom()], stroke);
        }
    }
    button.response.widget_info(|| egui::WidgetInfo::selected(egui::Role::Button, ui.is_enabled(), checked, name));
    button.response
}

#[cfg(test)]
mod color_tests;

#[cfg(test)]
mod tests;

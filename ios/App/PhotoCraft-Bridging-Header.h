#ifndef PHOTOCRAFT_BRIDGING_H
#define PHOTOCRAFT_BRIDGING_H

#include <stdint.h>
#include <stddef.h>

typedef struct PCCallbacks {
    void *user;
    void (*open_dialog)(void *user, int multiple, int pictures);
    void (*save_dialog)(void *user, const char *name);
    int (*write_file)(void *user, const char *path, const uint8_t *data, size_t len);
    void (*open_url)(void *user, const char *url);
    void (*copy_text)(void *user, const char *text);
} PCCallbacks;

void *pc_create(void *metal_layer, uint32_t width_px, uint32_t height_px, float scale, PCCallbacks cb);
void pc_destroy(void *h);
void pc_resize(void *h, uint32_t width_px, uint32_t height_px, float scale);
void pc_set_dark(void *h, int dark);
void pc_set_focused(void *h, int focused);
void pc_set_ui_scale(void *h, float zoom);
void pc_pointer_moved(void *h, float x, float y);
void pc_pointer_gone(void *h);
void pc_pointer_button(void *h, float x, float y, int button, int pressed, uint32_t mods);
void pc_pen(void *h, float x, float y, int phase, float pressure, float tilt_x, float tilt_y, float rotation, uint32_t mods, double timestamp);
void pc_pen_ex(void *h, float x, float y, int phase, float pressure, float tilt_x, float tilt_y, float rotation, uint32_t mods, double timestamp, uint64_t estimation_index, uint32_t expecting_mask);
void pc_pen_predicted(void *h, float x, float y, float pressure, float tilt_x, float tilt_y, float rotation, double timestamp);
void pc_pen_predicted_clear(void *h);
void pc_pen_update_estimate(void *h, float pressure, float tilt_x, float tilt_y, float rotation);
void pc_pen_update_estimate_ex(void *h, uint64_t estimation_index, float pressure, float tilt_x, float tilt_y, float rotation, uint32_t remaining_expected);
void pc_pen_hover(void *h, float x, float y, float tilt_x, float tilt_y, float rotation);
void pc_pen_hover_end(void *h);
void pc_touch_recorder_set_enabled(void *h, int enabled);
int pc_touch_recorder_is_enabled(void *h);
void pc_touch_recorder_record(void *h, double x, double y, float force, float max_force, int is_force_estimated, int64_t estimation_update_index, float altitude, float azimuth, double timestamp, uint8_t phase);
void pc_finger(void *h, int phase, float x, float y, uint32_t mods);
void pc_long_press(void *h, float x, float y);
void pc_click(void *h, float x, float y, int button, uint32_t mods);
void pc_rotate(void *h, float radians, float x, float y);
float pc_canvas_rotation(void *h);
float pc_canvas_zoom(void *h);
int pc_on_canvas(void *h, float x, float y);
void pc_set_touch_shift(void *h, int on);
void pc_select_tool(void *h, const char *name);
int pc_show_brush_options(void *h, float x, float y);
void pc_close_brush_picker(void *h);
void pc_scroll(void *h, float dx, float dy, uint32_t mods);
void pc_zoom(void *h, float factor, float x, float y);
void pc_key(void *h, uint32_t hid, int pressed, int repeat, uint32_t mods);
void pc_text(void *h, const char *utf8);
void pc_copy(void *h);
void pc_cut(void *h);
void pc_paste(void *h, const char *utf8);
void pc_open_file(void *h, const char *name, const uint8_t *data, size_t len);
void pc_dialog_file(void *h, const char *name, const uint8_t *data, size_t len);
void pc_dialog_path(void *h, const char *path);
void pc_dialog_end(void *h, int ok);
void pc_save_dialog_end(void *h, const char *path);
int pc_frame(void *h);
void pc_save_state(void *h);
int pc_is_dirty(void *h);
void pc_memory_warning(void *h);
void pc_telemetry_set_enabled(void *h, int enabled);
int pc_telemetry_get(void *h, float *frame_time_ms, float *touch_present_ms, float *gpu_tile_mem_mb, float *last_undo_ms);
int pc_is_fast_stroke_active(void *h);
void pc_sync_live_stroke_commit(void *h);
int pc_quickshape_trigger(void *h, int constrain);
int pc_quickshape_update(void *h, float x, float y, int constrain);
void pc_quickshape_commit(void *h);
float pc_apply_magnetic_rotation(float angle_deg, float detent_deg, int *snapped);

#endif

use crate::settings::{TimeFormat, ViewerMode};
use crate::video::metadata::VideoMetadata;

pub struct ViewerAction {
    pub overlay_placed: bool,
    pub center_percent_changed: bool,
}

/// Persistent state for the viewer zoom/pan.
pub struct ViewerZoomState {
    /// Zoom level: 1.0 = fit to viewport, higher = zoomed in.
    pub zoom: f32,
    /// Pan offset in image-space pixels (relative to center).
    pub pan_x: f32,
    pub pan_y: f32,
}

impl Default for ViewerZoomState {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            pan_x: 0.0,
            pan_y: 0.0,
        }
    }
}

impl ViewerZoomState {
    pub fn reset(&mut self) {
        self.zoom = 1.0;
        self.pan_x = 0.0;
        self.pan_y = 0.0;
    }

    /// Clamp pan so the image doesn't scroll entirely off-screen.
    fn clamp_pan(&mut self, display_w: f32, display_h: f32, viewport_w: f32, viewport_h: f32) {
        let max_pan_x = ((display_w - viewport_w) * 0.5).max(0.0);
        let max_pan_y = ((display_h - viewport_h) * 0.5).max(0.0);
        self.pan_x = self.pan_x.clamp(-max_pan_x, max_pan_x);
        self.pan_y = self.pan_y.clamp(-max_pan_y, max_pan_y);
    }
}

/// Render the main frame viewer panel.
pub fn render_viewer(
    ui: &mut egui::Ui,
    current_texture: Option<&egui::TextureHandle>,
    current_frame_missing: bool,
    current_frame: u32,
    total_frames: u32,
    metadata: Option<&VideoMetadata>,
    offset_seconds: f64,
    overlay_pos: &mut egui::Pos2,
    viewer_mode: ViewerMode,
    neighbor_textures: &[(u32, Option<egui::TextureHandle>, bool)],
    time_format: TimeFormat,
    zoom_state: &mut ViewerZoomState,
    frame_dimensions: Option<(u32, u32)>,
    filmstrip_before: u32,
    filmstrip_after: u32,
    center_percent: &mut u32,
) -> ViewerAction {
    match viewer_mode {
        ViewerMode::SingleFrame => {
            let overlay_placed = render_single_frame(ui, current_texture, current_frame_missing, current_frame, metadata, offset_seconds, overlay_pos, time_format, zoom_state, frame_dimensions);
            ViewerAction { overlay_placed, center_percent_changed: false }
        }
        ViewerMode::Filmstrip => {
            render_filmstrip(
                ui,
                current_texture,
                current_frame_missing,
                current_frame,
                total_frames,
                metadata,
                offset_seconds,
                overlay_pos,
                neighbor_textures,
                time_format,
                zoom_state,
                frame_dimensions,
                filmstrip_before,
                filmstrip_after,
                center_percent,
            )
        }
    }
}

fn render_single_frame(
    ui: &mut egui::Ui,
    texture: Option<&egui::TextureHandle>,
    frame_missing: bool,
    current_frame: u32,
    metadata: Option<&VideoMetadata>,
    offset_seconds: f64,
    overlay_pos: &mut egui::Pos2,
    time_format: TimeFormat,
    zoom_state: &mut ViewerZoomState,
    frame_dimensions: Option<(u32, u32)>,
) -> bool {
    let avail = ui.available_size();
    let mut overlay_placed = false;

    if let Some(tex) = texture {
        let tex_size = tex.size_vec2();
        let base_scale = (avail.x / tex_size.x).min(avail.y / tex_size.y).min(1.0);
        let scale = base_scale * zoom_state.zoom;
        let display_size = tex_size * scale;

        zoom_state.clamp_pan(display_size.x, display_size.y, avail.x, avail.y);

        let center_x = avail.x * 0.5 - zoom_state.pan_x;
        let center_y = avail.y * 0.5 - zoom_state.pan_y;

        let (rect, response) = ui.allocate_exact_size(avail, egui::Sense::click_and_drag());
        let image_rect = egui::Rect::from_center_size(
            rect.min + egui::vec2(center_x, center_y),
            display_size,
        );

        // Clip to viewport
        let clip_rect = rect;
        let painter = ui.painter().with_clip_rect(clip_rect);

        painter.image(
            tex.id(),
            image_rect,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );

        // Handle zoom/pan interactions
        handle_zoom_pan(ui, &response, &rect, zoom_state, base_scale, tex_size);

        if let Some(meta) = metadata {
            overlay_placed = render_time_overlay(ui, meta, current_frame, offset_seconds, overlay_pos, image_rect, time_format);
        }

        // Zoom indicator
        if zoom_state.zoom > 1.01 {
            render_zoom_indicator(ui, &rect, zoom_state.zoom);
        }
    } else if frame_missing {
        let image_rect = render_missing_frame(ui, avail, metadata, frame_dimensions);
        if let Some(meta) = metadata {
            overlay_placed = render_time_overlay(ui, meta, current_frame, offset_seconds, overlay_pos, image_rect, time_format);
        }
    } else {
        render_no_content(ui, metadata.is_some());
    }

    overlay_placed
}

fn render_filmstrip(
    ui: &mut egui::Ui,
    current_texture: Option<&egui::TextureHandle>,
    current_frame_missing: bool,
    current_frame: u32,
    total_frames: u32,
    metadata: Option<&VideoMetadata>,
    offset_seconds: f64,
    overlay_pos: &mut egui::Pos2,
    neighbor_textures: &[(u32, Option<egui::TextureHandle>, bool)],
    time_format: TimeFormat,
    zoom_state: &mut ViewerZoomState,
    frame_dimensions: Option<(u32, u32)>,
    before_count: u32,
    after_count: u32,
    center_percent: &mut u32,
) -> ViewerAction {
    let avail = ui.available_size();
    let mut center_percent_changed = false;

    if current_texture.is_none() && !current_frame_missing {
        render_no_content(ui, metadata.is_some());
        return ViewerAction { overlay_placed: false, center_percent_changed: false };
    }

    let spacing = 2.0;
    let frame_aspect = frame_dimensions
        .filter(|&(w, h)| w > 0 && h > 0)
        .map(|(w, h)| w as f32 / h as f32)
        .or_else(|| metadata.map(|m| m.width as f32 / m.height as f32))
        .unwrap_or(16.0 / 9.0);

    // Build slot lists for left and right grids.
    let left_slots: Vec<(Option<u32>, Option<&egui::TextureHandle>, bool)> = (1..=before_count)
        .rev()
        .map(|offset| {
            let idx = current_frame as i64 - offset as i64;
            if idx < 0 || idx >= total_frames as i64 {
                (None, None, false)
            } else {
                let idx = idx as u32;
                let entry = neighbor_textures.iter().find(|&&(i, _, _)| i == idx);
                match entry {
                    Some(&(_, ref tex, missing)) => (Some(idx), tex.as_ref(), missing),
                    None => (Some(idx), None, false),
                }
            }
        })
        .collect();

    let right_slots: Vec<(Option<u32>, Option<&egui::TextureHandle>, bool)> = (1..=after_count)
        .map(|offset| {
            let idx = current_frame as i64 + offset as i64;
            if idx < 0 || idx >= total_frames as i64 {
                (None, None, false)
            } else {
                let idx = idx as u32;
                let entry = neighbor_textures.iter().find(|&&(i, _, _)| i == idx);
                match entry {
                    Some(&(_, ref tex, missing)) => (Some(idx), tex.as_ref(), missing),
                    None => (Some(idx), None, false),
                }
            }
        })
        .collect();

    let left_count = left_slots.len();
    let right_count = right_slots.len();

    let min_ratio = *center_percent as f32 / 100.0;

    // Compute three-zone layout
    let zones = compute_filmstrip_zones(
        left_count, right_count, avail.x, avail.y, frame_aspect, spacing, min_ratio,
    );

    let (full_rect, response) = ui.allocate_exact_size(avail, egui::Sense::click_and_drag());

    // Use pointer position check (more reliable than response.hovered() which can be
    // blocked by overlapping interaction areas like the overlay drag region).
    let pointer_in_rect = ui.input(|i| {
        i.pointer.hover_pos().map_or(false, |p| full_rect.contains(p))
    });

    // Handle Ctrl+scroll (zoom), Shift+scroll (center percent), drag (pan)
    if pointer_in_rect {
        let scroll_y = ui.input(|i| i.raw_scroll_delta.y);
        let scroll_x = ui.input(|i| i.raw_scroll_delta.x);
        let ctrl = ui.input(|i| i.modifiers.ctrl);
        let shift = ui.input(|i| i.modifiers.shift);

        if ctrl && scroll_y.abs() > 0.1 {
            let mouse_pos = ui.input(|i| i.pointer.hover_pos()).unwrap_or(full_rect.center());
            let old_zoom = zoom_state.zoom;
            let factor = if scroll_y > 0.0 { 1.15 } else { 1.0 / 1.15 };
            zoom_state.zoom = (zoom_state.zoom * factor).clamp(1.0, 20.0);
            let new_zoom = zoom_state.zoom;

            // Adjust pan so point under mouse stays fixed (in UV space)
            let ref_w = zones.center_width.min(avail.y * frame_aspect);
            let ref_h = avail.y.min(zones.center_width / frame_aspect);
            let center_screen = full_rect.min + egui::vec2(zones.left_width + zones.center_width * 0.5, avail.y * 0.5);
            let mouse_off_x = (mouse_pos.x - center_screen.x) / ref_w;
            let mouse_off_y = (mouse_pos.y - center_screen.y) / ref_h;
            let ratio = 1.0 / new_zoom - 1.0 / old_zoom;
            zoom_state.pan_x -= mouse_off_x * ratio;
            zoom_state.pan_y -= mouse_off_y * ratio;

            let max_pan = (0.5 - 0.5 / zoom_state.zoom).max(0.0);
            zoom_state.pan_x = zoom_state.pan_x.clamp(-max_pan, max_pan);
            zoom_state.pan_y = zoom_state.pan_y.clamp(-max_pan, max_pan);
        } else if shift {
            // On Windows, Shift+Scroll sends horizontal scroll instead of vertical
            let scroll = if scroll_x.abs() > scroll_y.abs() { scroll_x } else { scroll_y };
            if scroll.abs() > 0.1 {
                let delta: i32 = if scroll > 0.0 { 10 } else { -10 };
                let new_val = (*center_percent as i32 + delta).clamp(110, 500) as u32;
                if new_val != *center_percent {
                    *center_percent = new_val;
                    center_percent_changed = true;
                }
            }
        }
    }

    // Drag to pan (when zoomed) — negate delta for "grab and drag" feel
    if zoom_state.zoom > 1.01 && response.dragged() {
        let delta = response.drag_delta();
        let ref_w = zones.center_width.min(avail.y * frame_aspect);
        let ref_h = avail.y.min(zones.center_width / frame_aspect);
        zoom_state.pan_x -= delta.x / (ref_w * zoom_state.zoom);
        zoom_state.pan_y -= delta.y / (ref_h * zoom_state.zoom);

        let max_pan = (0.5 - 0.5 / zoom_state.zoom).max(0.0);
        zoom_state.pan_x = zoom_state.pan_x.clamp(-max_pan, max_pan);
        zoom_state.pan_y = zoom_state.pan_y.clamp(-max_pan, max_pan);
    }

    // Double-click to reset zoom
    if response.double_clicked() {
        zoom_state.reset();
    }

    let zoom = zoom_state.zoom;
    let pan_x = zoom_state.pan_x;
    let pan_y = zoom_state.pan_y;

    let pad = 10.0; // padding between zones
    let left_rect = egui::Rect::from_min_size(
        full_rect.min,
        egui::vec2(zones.left_width, avail.y),
    );
    let center_rect = egui::Rect::from_min_size(
        egui::pos2(full_rect.min.x + zones.left_width + pad, full_rect.min.y + pad),
        egui::vec2((zones.center_width - pad * 2.0).max(0.0), (avail.y - pad * 2.0).max(0.0)),
    );
    let right_rect = egui::Rect::from_min_size(
        egui::pos2(full_rect.min.x + zones.left_width + zones.center_width, full_rect.min.y),
        egui::vec2(zones.right_width, avail.y),
    );

    let current_image_rect;
    let left_cells;
    let right_cells;
    {
        let painter = ui.painter();

        // Render center (current frame)
        current_image_rect = Some(render_grid_cell(
            painter, &center_rect, current_texture, current_frame_missing,
            Some(current_frame), frame_dimensions, true, zoom, pan_x, pan_y,
        ));

        // Render left neighbor grid
        left_cells = render_neighbor_grid(
            painter, &left_rect, &left_slots, zones.left_grid, spacing, frame_dimensions, zoom, pan_x, pan_y,
        );

        // Render right neighbor grid
        right_cells = render_neighbor_grid(
            painter, &right_rect, &right_slots, zones.right_grid, spacing, frame_dimensions, zoom, pan_x, pan_y,
        );
    }

    // Render overlays
    let mut overlay_placed = false;
    if let (Some(meta), Some(main_rect)) = (metadata, current_image_rect) {
        // Main frame overlay (draggable)
        overlay_placed = render_time_overlay(ui, meta, current_frame, offset_seconds, overlay_pos, main_rect, time_format);

        // Neighbor overlays (read-only, same relative position)
        let painter = ui.painter();
        for &(frame_idx, ref image_rect) in left_cells.iter().chain(right_cells.iter()) {
            render_overlay_readonly(
                painter, meta, frame_idx, offset_seconds, overlay_pos, &main_rect, image_rect, time_format,
            );
        }
    }

    // Zoom indicator
    if zoom_state.zoom > 1.01 {
        render_zoom_indicator(ui, &full_rect, zoom_state.zoom);
    }

    ViewerAction { overlay_placed, center_percent_changed }
}

/// Layout result for the three-zone filmstrip.
struct FilmstripZones {
    left_width: f32,
    center_width: f32,
    right_width: f32,
    left_grid: (usize, usize),  // (cols, rows)
    right_grid: (usize, usize),
}

/// Compute three-zone widths and sub-grid dimensions.
/// The center frame is guaranteed to be >= `min_ratio`x the linear size of neighbor frames.
/// Maximizes neighbor frame size within that constraint.
fn compute_filmstrip_zones(
    left_count: usize,
    right_count: usize,
    viewport_w: f32,
    viewport_h: f32,
    frame_aspect: f32,
    spacing: f32,
    min_ratio: f32,
) -> FilmstripZones {
    let has_left = left_count > 0;
    let has_right = right_count > 0;
    let num_sides = (has_left as usize) + (has_right as usize);

    if num_sides == 0 {
        return FilmstripZones {
            left_width: 0.0,
            center_width: viewport_w,
            right_width: 0.0,
            left_grid: (0, 0),
            right_grid: (0, 0),
        };
    }

    let mut best: Option<FilmstripZones> = None;
    let mut best_neighbor_h = 0.0f32;

    // Sweep per-side width from 5% to 45% in 0.5% steps
    for step in 10..=90 {
        let side_frac = step as f32 / 200.0;
        let side_w = viewport_w * side_frac;
        let center_w = viewport_w - side_w * num_sides as f32;

        if center_w < 40.0 || side_w < 20.0 {
            continue;
        }

        // Center frame linear size (height proxy)
        let c_h = (center_w / frame_aspect).min(viewport_h);

        // Compute side grids
        let lg = if has_left {
            compute_grid_layout(left_count, side_w, viewport_h, frame_aspect, spacing)
        } else {
            (0, 0)
        };
        let rg = if has_right {
            compute_grid_layout(right_count, side_w, viewport_h, frame_aspect, spacing)
        } else {
            (0, 0)
        };

        // Compute neighbor frame height for each side
        let frame_h_in = |cols: usize, rows: usize| -> f32 {
            if cols == 0 || rows == 0 {
                return 0.0;
            }
            let cw = (side_w - spacing * (cols as f32 - 1.0).max(0.0)) / cols as f32;
            let ch = (viewport_h - spacing * (rows as f32 - 1.0).max(0.0)) / rows as f32;
            (cw / frame_aspect).min(ch)
        };

        let l_h = frame_h_in(lg.0, lg.1);
        let r_h = frame_h_in(rg.0, rg.1);
        let max_nh = l_h.max(r_h);

        if max_nh <= 0.0 {
            continue;
        }

        // Enforce 1.5x constraint
        if c_h < min_ratio * max_nh {
            continue;
        }

        // Maximize neighbor frame size
        if max_nh > best_neighbor_h {
            best_neighbor_h = max_nh;
            best = Some(FilmstripZones {
                left_width: if has_left { side_w } else { 0.0 },
                center_width: center_w,
                right_width: if has_right { side_w } else { 0.0 },
                left_grid: lg,
                right_grid: rg,
            });
        }
    }

    // Fallback: 15% per side
    best.unwrap_or_else(|| {
        let side_w = viewport_w * 0.15;
        let center_w = viewport_w - side_w * num_sides as f32;
        let lg = if has_left {
            compute_grid_layout(left_count, side_w, viewport_h, frame_aspect, spacing)
        } else {
            (0, 0)
        };
        let rg = if has_right {
            compute_grid_layout(right_count, side_w, viewport_h, frame_aspect, spacing)
        } else {
            (0, 0)
        };
        FilmstripZones {
            left_width: if has_left { side_w } else { 0.0 },
            center_width: center_w,
            right_width: if has_right { side_w } else { 0.0 },
            left_grid: lg,
            right_grid: rg,
        }
    })
}

/// Compute the optimal (cols, rows) grid layout that maximizes frame display area.
fn compute_grid_layout(
    n: usize,
    viewport_w: f32,
    viewport_h: f32,
    frame_aspect: f32,
    spacing: f32,
) -> (usize, usize) {
    if n == 0 {
        return (1, 1);
    }

    let mut best_cols = 1usize;
    let mut best_rows = n;
    let mut best_area = 0.0f32;

    for cols in 1..=n {
        let rows = (n + cols - 1) / cols;

        let cell_w = (viewport_w - spacing * (cols as f32 - 1.0)) / cols as f32;
        let cell_h = (viewport_h - spacing * (rows as f32 - 1.0)) / rows as f32;

        if cell_w <= 0.0 || cell_h <= 0.0 {
            continue;
        }

        let cell_aspect = cell_w / cell_h;
        let (frame_w, frame_h) = if frame_aspect > cell_aspect {
            (cell_w, cell_w / frame_aspect)
        } else {
            (cell_h * frame_aspect, cell_h)
        };

        let area = frame_w * frame_h;
        if area > best_area {
            best_area = area;
            best_cols = cols;
            best_rows = rows;
        }
    }

    (best_cols, best_rows)
}

/// Render a grid of neighbor frames in the given area.
/// Returns (frame_idx, image_rect) for each valid cell (for overlay rendering).
fn render_neighbor_grid(
    painter: &egui::Painter,
    area: &egui::Rect,
    slots: &[(Option<u32>, Option<&egui::TextureHandle>, bool)],
    grid: (usize, usize),
    spacing: f32,
    frame_dimensions: Option<(u32, u32)>,
    zoom: f32,
    pan_x: f32,
    pan_y: f32,
) -> Vec<(u32, egui::Rect)> {
    let (cols, rows) = grid;
    if slots.is_empty() || cols == 0 || rows == 0 {
        return Vec::new();
    }

    let cell_w = (area.width() - spacing * (cols as f32 - 1.0).max(0.0)) / cols as f32;
    let cell_h = (area.height() - spacing * (rows as f32 - 1.0).max(0.0)) / rows as f32;

    let mut cells = Vec::new();
    for (i, &(frame_idx, tex, missing)) in slots.iter().enumerate() {
        let col = i % cols;
        let row = i / cols;

        let cell_x = area.min.x + col as f32 * (cell_w + spacing);
        let cell_y = area.min.y + row as f32 * (cell_h + spacing);
        let cell_rect = egui::Rect::from_min_size(
            egui::pos2(cell_x, cell_y),
            egui::vec2(cell_w, cell_h),
        );

        let image_rect = render_grid_cell(painter, &cell_rect, tex, missing, frame_idx, frame_dimensions, false, zoom, pan_x, pan_y);
        if let Some(idx) = frame_idx {
            cells.push((idx, image_rect));
        }
    }
    cells
}

/// Render a single frame in a grid cell. Returns the image rect.
/// `frame_idx` is None for out-of-bounds slots (renders "N/A").
/// `zoom`/`pan_x`/`pan_y` control UV-based zoom into the video content.
fn render_grid_cell(
    painter: &egui::Painter,
    cell_rect: &egui::Rect,
    texture: Option<&egui::TextureHandle>,
    missing: bool,
    frame_idx: Option<u32>,
    frame_dimensions: Option<(u32, u32)>,
    is_current: bool,
    zoom: f32,
    pan_x: f32,
    pan_y: f32,
) -> egui::Rect {
    // Out-of-bounds frame: black with "N/A"
    if frame_idx.is_none() {
        let rect = aspect_rect_within(cell_rect, frame_dimensions);
        painter.rect_filled(rect, 2.0, egui::Color32::from_gray(15));
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "N/A",
            egui::FontId::monospace(10.0),
            egui::Color32::WHITE,
        );
        return rect;
    }

    let idx = frame_idx.unwrap();

    let image_rect = if let Some(tex) = texture {
        let tex_size = tex.size_vec2();
        let scale = (cell_rect.width() / tex_size.x)
            .min(cell_rect.height() / tex_size.y)
            .min(1.0);
        let display_size = tex_size * scale;
        let offset = (cell_rect.size() - display_size) * 0.5;
        let rect = egui::Rect::from_min_size(cell_rect.min + offset, display_size);

        // Compute UV rect based on zoom/pan
        let uv_size = 1.0 / zoom;
        let uv_cx = 0.5 + pan_x;
        let uv_cy = 0.5 + pan_y;
        let uv_rect = egui::Rect::from_min_max(
            egui::pos2(
                (uv_cx - uv_size * 0.5).clamp(0.0, 1.0 - uv_size),
                (uv_cy - uv_size * 0.5).clamp(0.0, 1.0 - uv_size),
            ),
            egui::pos2(
                (uv_cx + uv_size * 0.5).clamp(uv_size, 1.0),
                (uv_cy + uv_size * 0.5).clamp(uv_size, 1.0),
            ),
        );

        // Clip to image rect to avoid overdraw
        let clipped_painter = painter.with_clip_rect(rect);
        clipped_painter.image(tex.id(), rect, uv_rect, egui::Color32::WHITE);

        let label = format!("F:{}", idx + 1);
        painter.text(
            egui::pos2(rect.center().x, rect.bottom() - 2.0),
            egui::Align2::CENTER_BOTTOM,
            label,
            egui::FontId::monospace(9.0),
            egui::Color32::from_white_alpha(200),
        );

        rect
    } else if missing {
        let rect = aspect_rect_within(cell_rect, frame_dimensions);
        painter.rect_filled(rect, 2.0, egui::Color32::BLACK);
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "MISSING",
            egui::FontId::monospace(10.0),
            egui::Color32::WHITE,
        );
        rect
    } else {
        // Loading placeholder
        painter.rect_filled(*cell_rect, 2.0, egui::Color32::from_gray(25));
        painter.text(
            cell_rect.center(),
            egui::Align2::CENTER_CENTER,
            format!("F:{}", idx + 1),
            egui::FontId::monospace(10.0),
            egui::Color32::from_gray(60),
        );
        *cell_rect
    };

    // Highlight border for the current frame
    if is_current {
        painter.rect_stroke(
            image_rect.expand(2.0),
            2.0,
            egui::Stroke::new(2.0, egui::Color32::from_rgb(255, 200, 0)),
            egui::epaint::StrokeKind::Outside,
        );
    }

    image_rect
}

/// Compute an aspect-ratio-correct rect centered within a container,
/// using frame_dimensions when available.
fn aspect_rect_within(
    container: &egui::Rect,
    frame_dimensions: Option<(u32, u32)>,
) -> egui::Rect {
    if let Some((w, h)) = frame_dimensions.filter(|&(w, h)| w > 0 && h > 0) {
        let scale = (container.width() / w as f32)
            .min(container.height() / h as f32)
            .min(1.0);
        let display_size = egui::vec2(w as f32 * scale, h as f32 * scale);
        let offset = (container.size() - display_size) * 0.5;
        egui::Rect::from_min_size(container.min + offset, display_size)
    } else {
        *container
    }
}

/// Render a black frame with "MISSING" text for the main viewer (allocates UI space).
/// Returns the image rect for overlay positioning.
fn render_missing_frame(
    ui: &mut egui::Ui,
    avail: egui::Vec2,
    metadata: Option<&VideoMetadata>,
    frame_dimensions: Option<(u32, u32)>,
) -> egui::Rect {
    let (rect, _) = ui.allocate_exact_size(avail, egui::Sense::hover());
    render_missing_frame_rect(ui.painter(), &rect, metadata, frame_dimensions)
}

/// Render a black frame with "MISSING" text into the given rect (painter only).
/// Uses actual frame dimensions (from cache/extraction) when available, falling
/// back to metadata dimensions. This ensures the placeholder matches real frame
/// size even when rotation swaps width/height.
fn render_missing_frame_rect(
    painter: &egui::Painter,
    container: &egui::Rect,
    metadata: Option<&VideoMetadata>,
    frame_dimensions: Option<(u32, u32)>,
) -> egui::Rect {
    // Prefer actual frame dimensions (accounts for rotation), fall back to metadata
    let dims = frame_dimensions
        .filter(|&(w, h)| w > 0 && h > 0)
        .or_else(|| metadata.map(|m| (m.width, m.height)));

    let image_rect = if let Some((w, h)) = dims {
        let tex_w = w as f32;
        let tex_h = h as f32;
        let scale = (container.width() / tex_w).min(container.height() / tex_h).min(1.0);
        let display_size = egui::vec2(tex_w * scale, tex_h * scale);
        let offset = (container.size() - display_size) * 0.5;
        egui::Rect::from_min_size(container.min + offset, display_size)
    } else {
        *container
    };

    painter.rect_filled(image_rect, 0.0, egui::Color32::BLACK);
    painter.text(
        image_rect.center(),
        egui::Align2::CENTER_CENTER,
        "MISSING",
        egui::FontId::monospace(24.0),
        egui::Color32::WHITE,
    );
    image_rect
}

fn render_no_content(ui: &mut egui::Ui, has_metadata: bool) {
    ui.centered_and_justified(|ui| {
        if has_metadata {
            ui.spinner();
            ui.label("Loading frame...");
        } else {
            ui.label("No recording loaded. Select a file from the browser.");
        }
    });
}

/// Handle Ctrl+scroll to zoom and drag to pan.
fn handle_zoom_pan(
    ui: &mut egui::Ui,
    response: &egui::Response,
    viewport: &egui::Rect,
    zoom_state: &mut ViewerZoomState,
    base_scale: f32,
    tex_size: egui::Vec2,
) {
    // Double-click to reset zoom
    if response.double_clicked() {
        zoom_state.reset();
        return;
    }

    // Ctrl+scroll to zoom centered on mouse position
    if response.hovered() {
        let scroll_delta = ui.input(|i| i.raw_scroll_delta.y);
        let ctrl = ui.input(|i| i.modifiers.ctrl);
        if ctrl && scroll_delta.abs() > 0.1 {
            let mouse_pos = ui
                .input(|i| i.pointer.hover_pos())
                .unwrap_or(viewport.center());

            // Mouse position relative to viewport center
            let mouse_off_x = mouse_pos.x - viewport.center().x + zoom_state.pan_x;
            let mouse_off_y = mouse_pos.y - viewport.center().y + zoom_state.pan_y;

            let old_zoom = zoom_state.zoom;
            let zoom_factor = if scroll_delta > 0.0 { 1.15 } else { 1.0 / 1.15 };
            zoom_state.zoom = (zoom_state.zoom * zoom_factor).clamp(1.0, 50.0);
            let new_zoom = zoom_state.zoom;

            // Adjust pan so the point under the mouse stays fixed
            let ratio = new_zoom / old_zoom;
            zoom_state.pan_x += mouse_off_x * (ratio - 1.0);
            zoom_state.pan_y += mouse_off_y * (ratio - 1.0);

            let display_size = tex_size * base_scale * new_zoom;
            zoom_state.clamp_pan(display_size.x, display_size.y, viewport.width(), viewport.height());
        }
    }

    // Drag to pan (only when zoomed in)
    if zoom_state.zoom > 1.01 && response.dragged() {
        let delta = response.drag_delta();
        zoom_state.pan_x -= delta.x;
        zoom_state.pan_y -= delta.y;

        let display_size = tex_size * base_scale * zoom_state.zoom;
        zoom_state.clamp_pan(display_size.x, display_size.y, viewport.width(), viewport.height());
    }
}

/// Draw a small zoom indicator at the bottom-right of the viewport.
fn render_zoom_indicator(ui: &mut egui::Ui, rect: &egui::Rect, zoom: f32) {
    let text = format!("{:.0}x", zoom);
    ui.painter().text(
        egui::pos2(rect.right() - 8.0, rect.bottom() - 8.0),
        egui::Align2::RIGHT_BOTTOM,
        text,
        egui::FontId::monospace(12.0),
        egui::Color32::from_white_alpha(160),
    );
}

/// Render a non-interactive time overlay on a neighbor frame.
/// Position is scaled proportionally from the main frame's overlay position.
fn render_overlay_readonly(
    painter: &egui::Painter,
    metadata: &VideoMetadata,
    frame_idx: u32,
    offset_seconds: f64,
    overlay_pos: &egui::Pos2,
    main_image_rect: &egui::Rect,
    neighbor_image_rect: &egui::Rect,
    time_format: TimeFormat,
) {
    let time = metadata.frame_to_time(frame_idx) + offset_seconds;
    let time_str = VideoMetadata::format_time(time, time_format);
    let fps = metadata.fps_at_frame(frame_idx);
    let display_text = format!("{time_str}  [{fps:.0} fps]  F:{}", frame_idx + 1);

    // Scale font size proportionally to frame height ratio
    let scale = (neighbor_image_rect.height() / main_image_rect.height()).min(1.0);
    let font_size = (16.0 * scale).max(7.0);
    let font = egui::FontId::monospace(font_size);

    let text_galley = painter.layout_no_wrap(display_text, font, egui::Color32::WHITE);
    let text_size = text_galley.size();
    let padding_scale = scale.max(0.3);
    let padding = egui::vec2(8.0 * padding_scale, 4.0 * padding_scale);
    let box_size = text_size + padding * 2.0;

    // Scale overlay position proportionally
    let frac_x = if main_image_rect.width() > 0.0 { overlay_pos.x / main_image_rect.width() } else { 0.0 };
    let frac_y = if main_image_rect.height() > 0.0 { overlay_pos.y / main_image_rect.height() } else { 0.0 };
    let abs_x = neighbor_image_rect.min.x + frac_x * neighbor_image_rect.width();
    let abs_y = neighbor_image_rect.min.y + frac_y * neighbor_image_rect.height();

    // Clamp so overlay stays within the neighbor frame
    let clamped_x = abs_x.min(neighbor_image_rect.max.x - box_size.x);
    let clamped_y = abs_y.min(neighbor_image_rect.max.y - box_size.y);
    let abs_pos = egui::pos2(clamped_x.max(neighbor_image_rect.min.x), clamped_y.max(neighbor_image_rect.min.y));

    let bg_rect = egui::Rect::from_min_size(abs_pos, box_size);

    // Clip to neighbor frame to avoid overdraw
    let clipped = painter.with_clip_rect(neighbor_image_rect.expand(1.0));
    clipped.rect_filled(bg_rect, 4.0 * scale, egui::Color32::from_black_alpha(180));
    clipped.galley(abs_pos + padding, text_galley, egui::Color32::WHITE);
}

/// Returns true if the overlay drag just ended (position should be saved).
fn render_time_overlay(
    ui: &mut egui::Ui,
    metadata: &VideoMetadata,
    current_frame: u32,
    offset_seconds: f64,
    overlay_pos: &mut egui::Pos2,
    image_rect: egui::Rect,
    time_format: TimeFormat,
) -> bool {
    let time = metadata.frame_to_time(current_frame) + offset_seconds;
    let time_str = VideoMetadata::format_time(time, time_format);
    let fps = metadata.fps_at_frame(current_frame);
    let display_text = format!("{time_str}  [{fps:.0} fps]  F:{}", current_frame + 1);

    let font = egui::FontId::monospace(16.0);
    let painter = ui.painter();

    let text_galley =
        painter.layout_no_wrap(display_text.clone(), font.clone(), egui::Color32::WHITE);
    let text_size = text_galley.size();
    let padding = egui::vec2(8.0, 4.0);
    let box_size = text_size + padding * 2.0;

    let abs_pos = egui::pos2(
        image_rect.min.x + overlay_pos.x,
        image_rect.min.y + overlay_pos.y,
    );

    let bg_rect = egui::Rect::from_min_size(abs_pos, box_size);

    painter.rect_filled(bg_rect, 4.0, egui::Color32::from_black_alpha(180));
    painter.galley(abs_pos + padding, text_galley, egui::Color32::WHITE);

    let response = ui.interact(bg_rect, ui.id().with("time_overlay"), egui::Sense::drag());
    if response.dragged() {
        let delta = response.drag_delta();
        overlay_pos.x += delta.x;
        overlay_pos.y += delta.y;

        overlay_pos.x = overlay_pos.x.clamp(0.0, image_rect.width() - box_size.x);
        overlay_pos.y = overlay_pos.y.clamp(0.0, image_rect.height() - box_size.y);
    }

    response.drag_stopped()
}

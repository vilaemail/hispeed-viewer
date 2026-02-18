use crate::settings::TimeFormat;
use crate::video::metadata::VideoMetadata;

/// Persistent state for the timeline zoom/scroll.
pub struct TimelineState {
    /// Zoom level: 1.0 = entire video visible, higher = zoomed in.
    pub zoom: f32,
    /// Scroll offset as fraction of total frames (0.0 = start, 1.0 = end).
    pub scroll: f32,
    /// True while the user is dragging the scrollbar thumb.
    scrollbar_dragging: bool,
}

impl Default for TimelineState {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            scroll: 0.0,
            scrollbar_dragging: false,
        }
    }
}

/// Render the timeline scrubber widget with zoom, X axis labels, and frame cells.
/// Returns the new frame index if the user clicked/dragged, or None.
pub fn render_timeline(
    ui: &mut egui::Ui,
    current_frame: u32,
    total_frames: u32,
    metadata: Option<&VideoMetadata>,
    cache_status: &dyn Fn(u32) -> bool,
    cached_frames: u32,
    time_format: TimeFormat,
    state: &mut TimelineState,
    offset_seconds: f64,
) -> Option<u32> {
    let mut new_frame = None;
    let avail = ui.available_size();

    let (rect, response) = ui.allocate_exact_size(avail, egui::Sense::click_and_drag());

    let painter = ui.painter_at(rect);

    // Background
    painter.rect_filled(rect, 0.0, egui::Color32::from_gray(20));

    if total_frames == 0 {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "No video loaded",
            egui::FontId::proportional(14.0),
            egui::Color32::from_gray(80),
        );
        return None;
    }

    // Layout: axis labels+ticks (26px), frame bar (fills middle), info text (14px), scrollbar (10px)
    let axis_height = 26.0;
    let text_row_height = 14.0;
    let scrollbar_height = 10.0;
    let bar_padding_x = 4.0;
    let tick_len = 7.0;

    let axis_rect = egui::Rect::from_min_size(
        egui::pos2(rect.left() + bar_padding_x, rect.top() + 2.0),
        egui::vec2(rect.width() - bar_padding_x * 2.0, axis_height),
    );
    let bar_rect = egui::Rect::from_min_max(
        egui::pos2(rect.left() + bar_padding_x, axis_rect.bottom()),
        egui::pos2(
            rect.right() - bar_padding_x,
            rect.bottom() - text_row_height - scrollbar_height - 4.0,
        ),
    );
    let bar_top = bar_rect.top();
    let bar_bottom = bar_rect.bottom();

    let scrollbar_rect = egui::Rect::from_min_max(
        egui::pos2(rect.left() + bar_padding_x, rect.bottom() - scrollbar_height - 1.0),
        egui::pos2(rect.right() - bar_padding_x, rect.bottom() - 1.0),
    );

    // Compute visible frame range from zoom/scroll
    let visible_frac = (1.0 / state.zoom).min(1.0);
    let max_scroll = (1.0 - visible_frac).max(0.0);
    state.scroll = state.scroll.clamp(0.0, max_scroll);

    let vis_start_frac = state.scroll;
    let vis_end_frac = (state.scroll + visible_frac).min(1.0);
    let vis_start_frame = (vis_start_frac * total_frames as f32) as u32;
    let vis_end_frame = ((vis_end_frac * total_frames as f32) as u32).min(total_frames);
    let vis_frame_count = vis_end_frame.saturating_sub(vis_start_frame).max(1);

    // Helper: frame index -> X coordinate
    let frame_to_x = |frame: u32| -> f32 {
        let t = (frame as f32 - vis_start_frame as f32) / vis_frame_count as f32;
        bar_rect.left() + t * bar_rect.width()
    };

    // Helper: X coordinate -> frame index
    // Uses floor() so clicking anywhere within a cell selects that cell's frame
    let x_to_frame = |x: f32| -> u32 {
        let t = ((x - bar_rect.left()) / bar_rect.width()).clamp(0.0, 1.0);
        let frame = vis_start_frame as f32 + t * vis_frame_count as f32;
        (frame.floor() as u32).clamp(0, total_frames.saturating_sub(1))
    };

    // Draw axis line at the bottom of axis area (top of frame bar)
    let axis_line_y = axis_rect.bottom();
    painter.line_segment(
        [
            egui::pos2(axis_rect.left(), axis_line_y),
            egui::pos2(axis_rect.right(), axis_line_y),
        ],
        egui::Stroke::new(1.0, egui::Color32::from_gray(100)),
    );

    // Draw X axis labels with tick marks above labels
    {
        let label_font = egui::FontId::monospace(9.0);
        let label_gap = 12.0; // minimum gap between adjacent labels

        // Measure a representative label to estimate spacing
        let sample_label = if let Some(meta) = metadata {
            let time = meta.frame_to_time(vis_start_frame) + offset_seconds;
            format!(
                "F:{} {}",
                vis_start_frame + 1,
                VideoMetadata::format_time(time, time_format)
            )
        } else {
            format!("F:{}", vis_end_frame)
        };
        let sample_galley =
            painter.layout_no_wrap(sample_label, label_font.clone(), egui::Color32::WHITE);
        let label_min_spacing = (sample_galley.size().x + label_gap).max(60.0);

        let approx_labels = (bar_rect.width() / label_min_spacing).max(2.0) as u32;
        let frame_step = nice_step(vis_frame_count, approx_labels);

        let first_tick = if vis_start_frame == 0 {
            0
        } else {
            ((vis_start_frame + frame_step - 1) / frame_step) * frame_step
        };

        let mut last_label_right = f32::NEG_INFINITY;
        let mut f = first_tick;
        while f < vis_end_frame {
            let x = frame_to_x(f);

            let label = if let Some(meta) = metadata {
                let time = meta.frame_to_time(f) + offset_seconds;
                format!(
                    "F:{} {}",
                    f + 1,
                    VideoMetadata::format_time(time, time_format)
                )
            } else {
                format!("F:{}", f + 1)
            };

            let galley = painter.layout_no_wrap(
                label,
                label_font.clone(),
                egui::Color32::from_gray(140),
            );
            let label_half_w = galley.size().x * 0.5;
            let label_left = x - label_half_w;

            // Only render tick + label if it won't overlap the previous label
            if label_left >= last_label_right {
                // Tick mark: upward from axis line
                painter.line_segment(
                    [
                        egui::pos2(x, axis_line_y - tick_len),
                        egui::pos2(x, axis_line_y),
                    ],
                    egui::Stroke::new(1.0, egui::Color32::from_gray(130)),
                );
                // Label above the tick mark
                let label_y = axis_line_y - tick_len - galley.size().y - 1.0;
                painter.galley(
                    egui::pos2(x - label_half_w, label_y.max(axis_rect.top())),
                    galley,
                    egui::Color32::from_gray(140),
                );
                last_label_right = x + label_half_w + label_gap;
            }

            f += frame_step;
        }
    }

    // Draw frame cells
    let frame_width = bar_rect.width() / vis_frame_count as f32;
    let bar_height = bar_bottom - bar_top;

    // Limit cell height to look proportional to width:
    // cap at frame_width * 2.5 (so cells aren't too tall and thin) with a minimum of 4px.
    let cell_height = if frame_width >= 4.0 {
        (frame_width * 2.5).clamp(4.0, bar_height)
    } else {
        bar_height // for thin/sampled views, use full height
    };
    let cell_y_offset = (bar_height - cell_height) * 0.5;
    let cell_top = bar_top + cell_y_offset;
    let cell_bottom = cell_top + cell_height;

    if frame_width >= 4.0 {
        // Individual frame cells with visible borders
        for i in vis_start_frame..vis_end_frame {
            let x = frame_to_x(i);
            let next_x = frame_to_x(i + 1);
            let gap = if frame_width > 8.0 { 1.0 } else { 0.5 };
            let cell_rect = egui::Rect::from_min_max(
                egui::pos2(x, cell_top),
                egui::pos2((next_x - gap).max(x + 1.0), cell_bottom),
            );

            let fill = if i == current_frame {
                egui::Color32::from_rgb(200, 70, 70)
            } else if cache_status(i) {
                egui::Color32::from_rgb(35, 75, 55)
            } else {
                egui::Color32::from_gray(35)
            };

            painter.rect_filled(cell_rect, 1.0, fill);

            // Show frame number inside cell when very zoomed in
            if frame_width > 40.0 {
                let label_color = if i == current_frame {
                    egui::Color32::WHITE
                } else {
                    egui::Color32::from_gray(100)
                };
                painter.text(
                    cell_rect.center(),
                    egui::Align2::CENTER_CENTER,
                    format!("{}", i + 1),
                    egui::FontId::monospace((frame_width * 0.3).clamp(8.0, 12.0)),
                    label_color,
                );
            }
        }
    } else if frame_width >= 0.5 {
        // Thin columns per frame
        for i in vis_start_frame..vis_end_frame {
            let x = frame_to_x(i);
            let color = if i == current_frame {
                egui::Color32::from_rgb(200, 70, 70)
            } else if cache_status(i) {
                egui::Color32::from_rgb(50, 100, 70)
            } else {
                egui::Color32::from_gray(35)
            };
            let w = frame_width.max(1.0);
            painter.rect_filled(
                egui::Rect::from_min_size(
                    egui::pos2(x, bar_top),
                    egui::vec2(w, bar_height),
                ),
                0.0,
                color,
            );
        }
    } else {
        // Sampled view: one pixel per column, sample the frame at that position
        let pixels = bar_rect.width() as u32;
        for px in 0..pixels {
            let x = bar_rect.left() + px as f32;
            let frame = x_to_frame(x);
            let color = if frame == current_frame {
                egui::Color32::from_rgb(200, 70, 70)
            } else if cache_status(frame) {
                egui::Color32::from_rgb(50, 100, 70)
            } else {
                egui::Color32::from_gray(35)
            };
            painter.rect_filled(
                egui::Rect::from_min_size(
                    egui::pos2(x, bar_top),
                    egui::vec2(1.0, bar_height),
                ),
                0.0,
                color,
            );
        }
    }

    // Playhead (drawn on top of frame cells)
    if current_frame >= vis_start_frame && current_frame < vis_end_frame {
        let head_x = frame_to_x(current_frame);

        // Only draw the line when frames are thin (otherwise the cell highlight is enough)
        if frame_width < 4.0 {
            painter.line_segment(
                [egui::pos2(head_x, bar_top), egui::pos2(head_x, bar_bottom)],
                egui::Stroke::new(2.0, egui::Color32::from_rgb(220, 80, 80)),
            );
        }

        // Triangle marker at top of bar area
        let tri_size = 5.0;
        painter.add(egui::Shape::convex_polygon(
            vec![
                egui::pos2(head_x - tri_size, bar_top),
                egui::pos2(head_x + tri_size, bar_top),
                egui::pos2(head_x, bar_top + tri_size),
            ],
            egui::Color32::from_rgb(220, 80, 80),
            egui::Stroke::NONE,
        ));
    }

    // --- Scrollbar ---
    let is_zoomed = state.zoom > 1.01;
    if is_zoomed {
        // Track background
        painter.rect_filled(scrollbar_rect, 3.0, egui::Color32::from_gray(35));

        // Thumb
        let thumb_frac = visible_frac; // fraction of track the thumb covers
        let track_w = scrollbar_rect.width();
        let thumb_w = (thumb_frac * track_w).max(20.0);
        let thumb_travel = track_w - thumb_w;
        let thumb_x = scrollbar_rect.left()
            + if max_scroll > 0.0 {
                state.scroll / max_scroll * thumb_travel
            } else {
                0.0
            };
        let thumb_rect = egui::Rect::from_min_size(
            egui::pos2(thumb_x, scrollbar_rect.top()),
            egui::vec2(thumb_w, scrollbar_rect.height()),
        );

        let thumb_hovered = response.hovered()
            && ui
                .input(|i| i.pointer.hover_pos())
                .is_some_and(|p| scrollbar_rect.contains(p));

        let thumb_color = if state.scrollbar_dragging {
            egui::Color32::from_gray(140)
        } else if thumb_hovered {
            egui::Color32::from_gray(110)
        } else {
            egui::Color32::from_gray(80)
        };
        painter.rect_filled(thumb_rect, 3.0, thumb_color);

        // Scrollbar drag interaction
        if response.drag_started() {
            if let Some(pos) = response.interact_pointer_pos() {
                if scrollbar_rect.contains(pos) {
                    state.scrollbar_dragging = true;
                }
            }
        }
        if !response.dragged() {
            state.scrollbar_dragging = false;
        }
        if state.scrollbar_dragging {
            if let Some(delta) = response.drag_delta().x.into() {
                if thumb_travel > 0.0 {
                    let scroll_delta: f32 = delta / thumb_travel * max_scroll;
                    state.scroll = (state.scroll + scroll_delta).clamp(0.0, max_scroll);
                }
            }
        }
    } else {
        state.scrollbar_dragging = false;
    }

    // Scroll wheel = zoom (Ctrl+scroll) or scroll (plain scroll)
    if response.hovered() {
        let scroll_delta = ui.input(|i| i.raw_scroll_delta.y);
        if scroll_delta.abs() > 0.1 {
            let ctrl = ui.input(|i| i.modifiers.ctrl);
            if ctrl {
                // Zoom: center on mouse position
                let mouse_x = ui
                    .input(|i| i.pointer.hover_pos().map(|p| p.x))
                    .unwrap_or(bar_rect.center().x);
                let mouse_t =
                    ((mouse_x - bar_rect.left()) / bar_rect.width()).clamp(0.0, 1.0);
                let mouse_frac = vis_start_frac + mouse_t * visible_frac;

                let zoom_factor = if scroll_delta > 0.0 { 1.15 } else { 1.0 / 1.15 };
                state.zoom =
                    (state.zoom * zoom_factor).clamp(1.0, total_frames as f32 / 2.0);

                // Adjust scroll to keep mouse position stable
                let new_vis_frac = (1.0 / state.zoom).min(1.0);
                state.scroll = (mouse_frac - mouse_t * new_vis_frac)
                    .clamp(0.0, (1.0 - new_vis_frac).max(0.0));
            } else {
                // Scroll the visible range
                let scroll_amount = -scroll_delta * visible_frac * 0.1;
                state.scroll = (state.scroll + scroll_amount).clamp(0.0, max_scroll);
            }
        }
    }

    // Click/drag to scrub (only if not dragging the scrollbar)
    if !state.scrollbar_dragging && (response.clicked() || response.dragged()) {
        if let Some(pos) = response.interact_pointer_pos() {
            // Only scrub if pointer is above the scrollbar area
            if pos.y < scrollbar_rect.top() {
                let frame = x_to_frame(pos.x);
                new_frame = Some(frame);
            }
        }
    }

    // Info text (between bar and scrollbar)
    let text_y = scrollbar_rect.top() - 2.0;
    let info_font = egui::FontId::monospace(11.0);
    let info_color = egui::Color32::from_gray(180);

    // Left: frame position
    painter.text(
        egui::pos2(rect.left() + 4.0, text_y),
        egui::Align2::LEFT_BOTTOM,
        format!("Frame: {} / {}", current_frame + 1, total_frames),
        info_font.clone(),
        info_color,
    );

    // Right: time position
    if let Some(meta) = metadata {
        let time = meta.frame_to_time(current_frame) + offset_seconds;
        let duration = meta.total_duration();
        painter.text(
            egui::pos2(rect.right() - 4.0, text_y),
            egui::Align2::RIGHT_BOTTOM,
            format!(
                "{} / {}",
                VideoMetadata::format_time(time, time_format),
                VideoMetadata::format_time(duration, time_format)
            ),
            info_font.clone(),
            info_color,
        );
    }

    // Center: cached frames count + zoom indicator
    {
        let pct = if total_frames > 0 {
            cached_frames as f32 / total_frames as f32 * 100.0
        } else {
            0.0
        };
        let mut center_text = format!("Cached: {}/{} ({:.0}%)", cached_frames, total_frames, pct);
        if is_zoomed {
            center_text = format!("{:.0}x  {}", state.zoom, center_text);
        }
        painter.text(
            egui::pos2(rect.center().x, text_y),
            egui::Align2::CENTER_BOTTOM,
            center_text,
            egui::FontId::monospace(9.0),
            egui::Color32::from_gray(120),
        );
    }

    new_frame
}

/// Choose a "nice" step size for axis labels given a range and desired label count.
fn nice_step(range: u32, desired_labels: u32) -> u32 {
    if range == 0 || desired_labels == 0 {
        return 1;
    }
    let raw = range / desired_labels;
    if raw <= 1 {
        return 1;
    }
    // Round to nearest nice number: 1, 2, 5, 10, 20, 50, 100, ...
    let magnitude = 10u32.pow((raw as f32).log10().floor() as u32);
    let normalized = raw as f32 / magnitude as f32;
    let nice = if normalized < 1.5 {
        1
    } else if normalized < 3.5 {
        2
    } else if normalized < 7.5 {
        5
    } else {
        10
    };
    (nice * magnitude).max(1)
}

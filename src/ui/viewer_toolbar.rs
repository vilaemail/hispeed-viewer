use crate::settings::{TimeFormat, ViewerMode};
use crate::ui::setting_widgets::{self, OffsetInlineAction, OffsetInlineState};
use crate::video::metadata::VideoMetadata;

/// Which speed field was last edited by the user.
#[derive(Clone, Copy, PartialEq)]
enum SpeedUnit {
    Fps,
    MsPerSec,
    X,
}

/// Persistent state for the three synced playback-speed text fields.
pub struct PlaybackSpeedState {
    fps_text: String,
    ms_per_sec_text: String,
    x_speed_text: String,
    /// Which field the user touched last (drives the sync direction).
    last_edited: SpeedUnit,
}

impl PlaybackSpeedState {
    pub fn new(default_fps: f64) -> Self {
        Self {
            fps_text: format_speed(default_fps),
            ms_per_sec_text: String::new(),
            x_speed_text: String::new(),
            last_edited: SpeedUnit::Fps,
        }
    }

    /// Recompute the text fields that the user did NOT just edit,
    /// given the canonical `playback_fps` and the video's original fps.
    pub fn sync_from_fps(&mut self, playback_fps: f64, original_fps: Option<f64>) {
        match self.last_edited {
            SpeedUnit::Fps => { /* keep fps_text as-is */ }
            _ => {
                self.fps_text = format_speed(playback_fps);
            }
        }
        if let Some(orig) = original_fps {
            if orig > 0.0 {
                let x = playback_fps / orig;
                let ms_s = x * 1000.0;
                if self.last_edited != SpeedUnit::MsPerSec {
                    self.ms_per_sec_text = format_speed(ms_s);
                }
                if self.last_edited != SpeedUnit::X {
                    self.x_speed_text = format_speed(x);
                }
                return;
            }
        }
        // No valid original fps — clear the dependent fields.
        if self.last_edited != SpeedUnit::MsPerSec {
            self.ms_per_sec_text = String::new();
        }
        if self.last_edited != SpeedUnit::X {
            self.x_speed_text = String::new();
        }
    }
}

/// Format a speed value: drop unnecessary trailing zeros, keep up to 6 decimals.
fn format_speed(v: f64) -> String {
    if v == 0.0 {
        return "0".into();
    }
    // Use enough precision to round-trip, then strip trailing zeros.
    let s = format!("{:.6}", v);
    let s = s.trim_end_matches('0');
    let s = s.trim_end_matches('.');
    s.to_string()
}

/// Actions returned by the viewer toolbar for the caller to handle.
pub struct ToolbarActions {
    pub time_format_changed: bool,
    pub viewer_mode_changed: bool,
    pub offset_action: OffsetInlineAction,
    pub playback_toggled: bool,
    pub playback_fps_changed: Option<f64>,
}

/// Render the contextual toolbar above the viewer.
/// Shows view mode, time format, offset controls, and playback controls.
pub fn render_viewer_toolbar(
    ui: &mut egui::Ui,
    viewer_mode: &mut ViewerMode,
    time_format: &mut TimeFormat,
    offset_state: &mut OffsetInlineState,
    current_offset: f64,
    current_frame: u32,
    metadata: Option<&VideoMetadata>,
    is_playing: bool,
    speed_state: &mut PlaybackSpeedState,
) -> ToolbarActions {
    let mut actions = ToolbarActions {
        time_format_changed: false,
        viewer_mode_changed: false,
        offset_action: OffsetInlineAction::None,
        playback_toggled: false,
        playback_fps_changed: None,
    };

    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;

        // --- Play / Pause button ---
        let btn_label = if is_playing { "\u{23f8} Pause" } else { "\u{25b6} Play" };
        if ui.button(btn_label).clicked() {
            actions.playback_toggled = true;
        }

        ui.separator();

        // --- Speed fields ---
        let original_fps = metadata.map(|m| m.fps);
        let fields_enabled = !is_playing;

        // FPS field
        ui.label("FPS:");
        let fps_resp = ui.add_enabled(
            fields_enabled,
            egui::TextEdit::singleline(&mut speed_state.fps_text)
                .desired_width(60.0)
                .hint_text("30"),
        );
        if fps_resp.changed() {
            speed_state.last_edited = SpeedUnit::Fps;
            if let Ok(v) = speed_state.fps_text.parse::<f64>() {
                if v > 0.0 {
                    actions.playback_fps_changed = Some(v);
                    speed_state.sync_from_fps(v, original_fps);
                }
            }
        }

        // ms/s field
        ui.label("ms/s:");
        let has_original = original_fps.map_or(false, |f| f > 0.0);
        let ms_resp = ui.add_enabled(
            fields_enabled && has_original,
            egui::TextEdit::singleline(&mut speed_state.ms_per_sec_text)
                .desired_width(60.0)
                .hint_text("—"),
        );
        if ms_resp.changed() {
            speed_state.last_edited = SpeedUnit::MsPerSec;
            if let (Ok(ms), Some(orig)) = (speed_state.ms_per_sec_text.parse::<f64>(), original_fps) {
                if ms > 0.0 && orig > 0.0 {
                    let new_fps = ms * orig / 1000.0;
                    actions.playback_fps_changed = Some(new_fps);
                    speed_state.sync_from_fps(new_fps, original_fps);
                }
            }
        }

        // x field
        ui.label("x:");
        let x_resp = ui.add_enabled(
            fields_enabled && has_original,
            egui::TextEdit::singleline(&mut speed_state.x_speed_text)
                .desired_width(60.0)
                .hint_text("—"),
        );
        if x_resp.changed() {
            speed_state.last_edited = SpeedUnit::X;
            if let (Ok(x), Some(orig)) = (speed_state.x_speed_text.parse::<f64>(), original_fps) {
                if x > 0.0 && orig > 0.0 {
                    let new_fps = x * orig;
                    actions.playback_fps_changed = Some(new_fps);
                    speed_state.sync_from_fps(new_fps, original_fps);
                }
            }
        }

        ui.separator();

        // --- View mode ---
        ui.label("View:");
        actions.viewer_mode_changed =
            setting_widgets::viewer_mode_combo(ui, "toolbar_viewer_mode", viewer_mode);

        ui.separator();

        // --- Time format ---
        ui.label("Time:");
        actions.time_format_changed =
            setting_widgets::time_format_combo(ui, "toolbar_time_format", time_format);

        ui.separator();

        // --- Offset ---
        actions.offset_action = setting_widgets::offset_inline(
            ui,
            offset_state,
            current_offset,
            current_frame,
            metadata,
        );
    });

    ui.separator();

    actions
}

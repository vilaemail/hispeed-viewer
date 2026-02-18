use crate::settings::{TimeFormat, ViewerMode};
use crate::video::metadata::VideoMetadata;

/// Render a ComboBox for TimeFormat selection.
/// `id_salt` must be unique per call site to avoid egui ID collisions.
/// Returns true if the value changed.
pub fn time_format_combo(ui: &mut egui::Ui, id_salt: &str, current: &mut TimeFormat) -> bool {
    let before = *current;
    egui::ComboBox::from_id_salt(id_salt)
        .selected_text(current.label())
        .show_ui(ui, |ui| {
            for &fmt in TimeFormat::ALL {
                ui.selectable_value(current, fmt, fmt.label());
            }
        });
    *current != before
}

/// Render a ComboBox for ViewerMode selection.
/// `id_salt` must be unique per call site to avoid egui ID collisions.
/// Returns true if the value changed.
pub fn viewer_mode_combo(ui: &mut egui::Ui, id_salt: &str, current: &mut ViewerMode) -> bool {
    let before = *current;
    egui::ComboBox::from_id_salt(id_salt)
        .selected_text(current.label())
        .show_ui(ui, |ui| {
            for &mode in ViewerMode::ALL {
                ui.selectable_value(current, mode, mode.label());
            }
        });
    *current != before
}

/// Persistent state for the inline offset time inputs.
pub struct OffsetInlineState {
    pub seconds: String,
    pub ms: String,
    pub us: String,
}

impl Default for OffsetInlineState {
    fn default() -> Self {
        Self {
            seconds: "0".into(),
            ms: "0".into(),
            us: "0".into(),
        }
    }
}

/// Action returned by the inline offset control.
pub enum OffsetInlineAction {
    None,
    /// Offset was computed and should be set to this value.
    Applied(f64),
    /// Offset should be reset to zero.
    Reset,
}

/// Render an inline offset control: current offset display, time inputs for
/// the current frame, "Set Offset" and "Reset" buttons.
pub fn offset_inline(
    ui: &mut egui::Ui,
    state: &mut OffsetInlineState,
    current_offset: f64,
    current_frame: u32,
    metadata: Option<&VideoMetadata>,
) -> OffsetInlineAction {
    let mut action = OffsetInlineAction::None;
    let has_video = metadata.is_some();

    ui.label(
        egui::RichText::new(format!("Offset: {current_offset:.6}s"))
            .monospace()
            .size(11.0)
            .color(egui::Color32::from_gray(170)),
    );

    ui.add_enabled_ui(has_video, |ui| {
        let w = 40.0;
        ui.add(egui::TextEdit::singleline(&mut state.seconds).desired_width(w));
        ui.label("s");
        ui.add(egui::TextEdit::singleline(&mut state.ms).desired_width(w));
        ui.label("ms");
        ui.add(egui::TextEdit::singleline(&mut state.us).desired_width(w));
        ui.label("us");

        if ui.small_button("Set Offset").clicked() {
            if let Some(meta) = metadata {
                let secs: f64 = state.seconds.trim().parse().unwrap_or(0.0);
                let ms: f64 = state.ms.trim().parse().unwrap_or(0.0);
                let us: f64 = state.us.trim().parse().unwrap_or(0.0);
                let desired_time = secs + ms / 1_000.0 + us / 1_000_000.0;
                let raw_time = meta.frame_to_time(current_frame);
                action = OffsetInlineAction::Applied(desired_time - raw_time);
            }
        }
        if ui.small_button("Reset").clicked() {
            action = OffsetInlineAction::Reset;
        }
    });

    action
}

use crate::video::metadata::VideoMetadata;

/// Transient state for the "Set Offset" window.
pub struct SetOffsetState {
    pub frame_input: String,
    pub time_input: String,
    pub message: Option<String>,
}

impl Default for SetOffsetState {
    fn default() -> Self {
        Self {
            frame_input: "1".into(),
            time_input: "0.000".into(),
            message: None,
        }
    }
}

/// Action returned by the offset window.
pub enum OffsetAction {
    /// No change.
    None,
    /// Offset was changed to this value.
    Applied(f64),
    /// Offset was reset to zero.
    Reset,
}

/// Render the "Set Offset" window contents.
/// Returns an OffsetAction if the offset changed.
pub fn render_set_offset(
    ui: &mut egui::Ui,
    state: &mut SetOffsetState,
    current_frame: u32,
    current_offset: f64,
    metadata: Option<&VideoMetadata>,
) -> OffsetAction {
    let mut action = OffsetAction::None;

    ui.label("Set Frame Time:");
    ui.label(
        egui::RichText::new(
            "Define what time a particular frame should show. This sets the time offset.",
        )
        .small()
        .color(egui::Color32::from_gray(140)),
    );

    ui.add_space(4.0);

    ui.horizontal(|ui| {
        ui.label("Frame:");
        ui.add(egui::TextEdit::singleline(&mut state.frame_input).desired_width(60.0));
        ui.label("= Time (seconds):");
        ui.add(egui::TextEdit::singleline(&mut state.time_input).desired_width(80.0));
    });

    ui.add_space(4.0);

    ui.horizontal(|ui| {
        if ui.button("Apply").clicked() {
            if let Some(meta) = metadata {
                if let (Ok(frame_1based), Ok(desired_time)) = (
                    state.frame_input.trim().parse::<u32>(),
                    state.time_input.trim().parse::<f64>(),
                ) {
                    if frame_1based >= 1 && frame_1based <= meta.total_frames {
                        let frame_0based = frame_1based - 1;
                        let raw_time = meta.frame_to_time(frame_0based);
                        let new_offset = desired_time - raw_time;
                        state.message = Some(format!("Offset set to {new_offset:.6}s"));
                        action = OffsetAction::Applied(new_offset);
                    } else {
                        state.message =
                            Some(format!("Frame must be 1-{}", meta.total_frames));
                    }
                } else {
                    state.message = Some("Invalid number".into());
                }
            } else {
                state.message = Some("No video loaded".into());
            }
        }
        if ui.button("Reset").clicked() {
            state.message = Some("Offset reset to 0".into());
            action = OffsetAction::Reset;
        }
    });

    ui.add_space(4.0);

    ui.horizontal(|ui| {
        ui.label("Current frame:");
        ui.label(
            egui::RichText::new(format!("F:{}", current_frame + 1))
                .monospace()
                .color(egui::Color32::from_gray(180)),
        );
        if ui.small_button("Use current").clicked() {
            state.frame_input = format!("{}", current_frame + 1);
        }
    });

    ui.label(
        egui::RichText::new(format!("Current offset: {current_offset:.6}s"))
            .monospace()
            .small()
            .color(egui::Color32::from_gray(160)),
    );

    if let Some(msg) = &state.message {
        ui.label(
            egui::RichText::new(msg)
                .small()
                .color(egui::Color32::from_gray(160)),
        );
    }

    action
}

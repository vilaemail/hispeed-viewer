use crate::source::traits::SourceStatus;

/// Render the sync status indicator in the menu bar or status area.
pub fn render_sync_status(ui: &mut egui::Ui, status: SourceStatus, source_name: &str) {
    let (color, label) = match status {
        SourceStatus::Available => (egui::Color32::from_rgb(60, 180, 60), "Connected"),
        SourceStatus::Unavailable => (egui::Color32::from_rgb(180, 60, 60), "Disconnected"),
        SourceStatus::Checking => (egui::Color32::from_rgb(180, 180, 60), "Checking..."),
    };

    ui.horizontal(|ui| {
        // Status dot
        let (dot_rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
        ui.painter().circle_filled(dot_rect.center(), 4.0, color);
        ui.label(format!("{source_name}: {label}"));
    });
}

/// Render the update approval dialog.
/// Returns a list of approved change indices.
pub fn render_update_approval(
    ui: &mut egui::Ui,
    pending_changes: &[PendingChange],
) -> Vec<usize> {
    let mut approved = Vec::new();

    ui.heading("Remote Updates Available");
    ui.separator();

    if pending_changes.is_empty() {
        ui.label("No pending updates.");
        return approved;
    }

    for (i, change) in pending_changes.iter().enumerate() {
        ui.group(|ui| {
            ui.label(egui::RichText::new(&change.file_name).strong());
            ui.label(&change.description);

            ui.horizontal(|ui| {
                ui.label(format!("Old size: {}", change.old_size));
                ui.label(format!("New size: {}", change.new_size));
            });

            if ui.button("Approve").clicked() {
                approved.push(i);
            }
        });
        ui.add_space(4.0);
    }

    approved
}

/// Describes a pending file change awaiting user approval.
#[derive(Debug, Clone)]
pub struct PendingChange {
    pub file_name: String,
    pub description: String,
    pub old_size: String,
    pub new_size: String,
}

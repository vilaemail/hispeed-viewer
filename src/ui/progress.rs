use crate::video::extractor::ExtractionProgress;

/// Render a modal-style progress overlay for frame extraction.
/// Returns true if the user clicked Cancel.
pub fn render_extraction_progress(ctx: &egui::Context, progress: &ExtractionProgress) -> bool {
    let mut cancelled = false;

    egui::Area::new(egui::Id::new("extraction_overlay"))
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_min_width(300.0);

                ui.heading("Extracting Frames...");
                ui.add_space(8.0);

                let bar = egui::ProgressBar::new(progress.fraction())
                    .text(format!(
                        "{} / {} frames",
                        progress.extracted_frames, progress.total_frames
                    ))
                    .animate(true);
                ui.add(bar);

                ui.add_space(8.0);

                if ui.button("Cancel").clicked() {
                    cancelled = true;
                }
            });
        });

    cancelled
}

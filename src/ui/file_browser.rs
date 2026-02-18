use std::path::Path;

/// A completed recording available for viewing.
#[derive(Debug, Clone)]
pub struct CompletedRecording {
    pub json_path: std::path::PathBuf,
    pub video_path: std::path::PathBuf,
    pub display_name: String,
    pub file_size: u64,
}

/// Scan the completed directory for available recordings.
pub fn scan_completed_dir(completed_dir: &Path) -> Vec<CompletedRecording> {
    let mut recordings = Vec::new();

    let entries = match std::fs::read_dir(completed_dir) {
        Ok(e) => e,
        Err(_) => return recordings,
    };

    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "json") {
            // Look for matching MP4
            let stem = path.file_stem().unwrap_or_default().to_string_lossy();
            let video_path = path.with_extension("mp4");

            if video_path.exists() {
                let file_size = std::fs::metadata(&video_path)
                    .map(|m| m.len())
                    .unwrap_or(0);

                recordings.push(CompletedRecording {
                    json_path: path.clone(),
                    video_path,
                    display_name: stem.into_owned(),
                    file_size,
                });
            }
        }
    }

    recordings.sort_by(|a, b| b.display_name.cmp(&a.display_name)); // newest first
    recordings
}

/// Render the file browser panel.
/// Returns the index of the recording the user clicked to open, if any.
pub fn render_file_browser(
    ui: &mut egui::Ui,
    recordings: &[CompletedRecording],
    current_recording: Option<usize>,
) -> Option<usize> {
    let mut selected = None;

    ui.heading("Recordings");
    ui.separator();

    if recordings.is_empty() {
        ui.label("No recordings found in completed folder.");
        ui.label("Configure source and sync to fetch recordings.");
        return None;
    }

    egui::ScrollArea::vertical().show(ui, |ui| {
        for (i, rec) in recordings.iter().enumerate() {
            let is_current = current_recording == Some(i);
            let text = format!(
                "{}\n{}",
                rec.display_name,
                format_file_size(rec.file_size)
            );

            let response = ui.selectable_label(is_current, &text);
            if response.clicked() && !is_current {
                selected = Some(i);
            }
        }
    });

    selected
}

fn format_file_size(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

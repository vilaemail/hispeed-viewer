use std::path::Path;

use crate::video::metadata::VideoMetadata;

/// A completed recording available for viewing.
#[derive(Debug, Clone)]
pub struct CompletedRecording {
    pub json_path: std::path::PathBuf,
    pub video_path: std::path::PathBuf,
    /// The original filename (json stem).
    pub original_name: String,
    /// The name to display (custom if set, otherwise original).
    pub display_name: String,
    /// Whether this recording has a custom name override.
    pub has_custom_name: bool,
    pub file_size: u64,
    /// Full video metadata for the properties window.
    pub metadata: VideoMetadata,
    /// Whether frames are currently cached on disk.
    pub is_cached: bool,
}

/// Action returned by the file browser.
pub enum FileBrowserAction {
    None,
    Open(usize),
    Rename { index: usize, new_name: String },
    RestoreOriginalName(usize),
}

/// State for the rename popup window.
#[derive(Default)]
pub struct RenameState {
    /// Index of the recording being renamed.
    pub target: Option<usize>,
    /// Text buffer for the rename input.
    pub buffer: String,
}

/// State for the properties popup window.
#[derive(Default)]
pub struct PropertiesState {
    /// Index of the recording to show properties for.
    pub target: Option<usize>,
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

                if let Ok(meta) = VideoMetadata::from_file(&path) {
                    recordings.push(CompletedRecording {
                        json_path: path.clone(),
                        video_path,
                        original_name: stem.to_string(),
                        display_name: stem.into_owned(),
                        has_custom_name: false,
                        file_size,
                        metadata: meta,
                        is_cached: false,
                    });
                }
            }
        }
    }

    recordings.sort_by(|a, b| b.display_name.cmp(&a.display_name)); // newest first
    recordings
}

/// Render the file browser panel.
/// Returns a `FileBrowserAction` indicating what the user did.
pub fn render_file_browser(
    ui: &mut egui::Ui,
    recordings: &[CompletedRecording],
    current_recording: Option<usize>,
    sort_by_original: &mut bool,
    rename_state: &mut RenameState,
    properties_state: &mut PropertiesState,
) -> FileBrowserAction {
    let mut action = FileBrowserAction::None;

    // Header row: "Recordings" + sort button
    ui.horizontal(|ui| {
        ui.heading("Recordings");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let sort_btn = ui.small_button("Sort");
            if sort_btn.clicked() {
                ui.memory_mut(|mem| mem.toggle_popup(sort_btn.id));
            }
            egui::popup_below_widget(ui, sort_btn.id, &sort_btn, egui::PopupCloseBehavior::CloseOnClick, |ui| {
                ui.set_min_width(120.0);
                if ui.selectable_label(*sort_by_original, "Original Names").clicked() {
                    *sort_by_original = true;
                }
                if ui.selectable_label(!*sort_by_original, "Custom Names").clicked() {
                    *sort_by_original = false;
                }
            });
        });
    });
    ui.separator();

    if recordings.is_empty() {
        ui.label("No recordings found in completed folder.");
        ui.label("Configure source and sync to fetch recordings.");
        return FileBrowserAction::None;
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
                action = FileBrowserAction::Open(i);
            }

            // Right-click context menu
            response.context_menu(|ui| {
                if ui.button("Rename").clicked() {
                    rename_state.target = Some(i);
                    rename_state.buffer = rec.display_name.clone();
                    ui.close_menu();
                }
                if rec.has_custom_name {
                    if ui.button("Restore Original Name").clicked() {
                        action = FileBrowserAction::RestoreOriginalName(i);
                        ui.close_menu();
                    }
                }
                ui.separator();
                if ui.button("Properties").clicked() {
                    properties_state.target = Some(i);
                    ui.close_menu();
                }
            });
        }
    });

    // Rename popup window
    if rename_state.target.is_some() {
        let mut open = true;
        egui::Window::new("Rename Recording")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ui.ctx(), |ui| {
                ui.label("Enter new name:");
                let response = ui.text_edit_singleline(&mut rename_state.buffer);
                if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    let name = rename_state.buffer.trim().to_string();
                    if let Some(idx) = rename_state.target.take() {
                        if !name.is_empty() {
                            action = FileBrowserAction::Rename { index: idx, new_name: name };
                        }
                    }
                }
                ui.horizontal(|ui| {
                    if ui.button("OK").clicked() {
                        let name = rename_state.buffer.trim().to_string();
                        if let Some(idx) = rename_state.target.take() {
                            if !name.is_empty() {
                                action = FileBrowserAction::Rename { index: idx, new_name: name };
                            }
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        rename_state.target = None;
                    }
                });
            });
        if !open {
            rename_state.target = None;
        }
    }

    // Properties popup window
    if let Some(idx) = properties_state.target {
        if let Some(rec) = recordings.get(idx) {
            let mut open = true;
            let title = format!("Properties - {}", rec.display_name);
            egui::Window::new(title)
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .default_width(320.0)
                .show(ui.ctx(), |ui| {
                    let meta = &rec.metadata;

                    egui::Grid::new("properties_grid")
                        .num_columns(2)
                        .spacing([12.0, 4.0])
                        .show(ui, |ui| {
                            let label = |ui: &mut egui::Ui, text: &str| {
                                ui.label(egui::RichText::new(text).color(egui::Color32::from_gray(160)));
                            };

                            if rec.has_custom_name {
                                label(ui, "Custom Name");
                                ui.label(&rec.display_name);
                                ui.end_row();

                                label(ui, "Original Name");
                                ui.label(&rec.original_name);
                                ui.end_row();
                            } else {
                                label(ui, "Name");
                                ui.label(&rec.original_name);
                                ui.end_row();
                            }

                            label(ui, "File");
                            ui.label(&meta.file);
                            ui.end_row();

                            label(ui, "Resolution");
                            ui.label(format!("{} x {}", meta.width, meta.height));
                            ui.end_row();

                            label(ui, "Total Frames");
                            ui.label(format!("{}", meta.total_frames));
                            ui.end_row();

                            if !meta.engine.is_empty() {
                                label(ui, "Engine");
                                ui.label(&meta.engine);
                                ui.end_row();
                            }

                            label(ui, "File Size");
                            ui.label(format_file_size(meta.size));
                            ui.end_row();

                            label(ui, "SHA-256");
                            ui.label(egui::RichText::new(&meta.sha256).monospace().size(11.0));
                            ui.end_row();

                            label(ui, "Cached");
                            if rec.is_cached {
                                ui.label(egui::RichText::new("Yes").color(egui::Color32::from_rgb(80, 180, 80)));
                            } else {
                                ui.label(egui::RichText::new("No").color(egui::Color32::from_gray(140)));
                            }
                            ui.end_row();
                        });

                    // Frame segments
                    if !meta.frames.is_empty() {
                        ui.add_space(8.0);
                        ui.separator();
                        ui.label(egui::RichText::new("Frame Segments").color(egui::Color32::from_gray(160)));
                        egui::Grid::new("segments_grid")
                            .num_columns(2)
                            .spacing([12.0, 2.0])
                            .show(ui, |ui| {
                                for seg in &meta.frames {
                                    ui.label(format!("{} frames", seg.count));
                                    ui.label(format!("{} fps", seg.fps));
                                    ui.end_row();
                                }
                            });
                    }
                });
            if !open {
                properties_state.target = None;
            }
        } else {
            properties_state.target = None;
        }
    }

    action
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

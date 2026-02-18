use crate::settings::{CacheStorageMode, CompressionAlgorithm, CompressionDevice, Settings, SourceType};
use crate::ui::setting_widgets;
use crate::video::gpu_device::GpuAvailability;
use std::path::{Path, PathBuf};

/// Describes a pending folder move when the user changes a storage path.
pub struct PendingFolderMove {
    pub label: String,
    pub old_path: PathBuf,
    pub new_path: PathBuf,
    pub has_files: bool,
}

/// Which benchmark mode the user selected.
#[derive(Clone, Copy, PartialEq, Default)]
pub enum BenchmarkMode {
    #[default]
    CurrentSettings,
    SmallSubset,
    All,
}

impl BenchmarkMode {
    pub const ALL: &[BenchmarkMode] = &[Self::CurrentSettings, Self::SmallSubset, Self::All];

    pub fn label(&self) -> &'static str {
        match self {
            Self::CurrentSettings => "Current Settings",
            Self::SmallSubset => "Small Subset",
            Self::All => "All Combinations",
        }
    }
}

/// State for the settings validation/move-confirmation dialog.
#[derive(Default)]
pub struct SettingsValidationState {
    pub errors: Vec<String>,
    pub showing_validation: bool,
    pub pending_moves: Vec<PendingFolderMove>,
    pub showing_moves: bool,
    pub selected_benchmark_mode: BenchmarkMode,
}

/// Action returned by the settings window.
pub enum SettingsAction {
    /// No action needed.
    None,
    /// User wants to save (validated OK or chose "Save Anyway").
    Save,
    /// User wants to run a benchmark.
    Benchmark(BenchmarkMode),
}

/// Render the settings window.
/// Returns a SettingsAction indicating whether to save.
pub fn render_settings(
    ui: &mut egui::Ui,
    settings: &mut Settings,
    old_settings: &Settings,
    exe_dir: &Path,
    validation: &mut SettingsValidationState,
    gpu_availability: &GpuAvailability,
) -> SettingsAction {
    let mut action = SettingsAction::None;

    egui::ScrollArea::vertical().show(ui, |ui| {
    egui::Frame::NONE
        .inner_margin(egui::Margin { left: 8, right: 16, top: 4, bottom: 8 })
        .show(ui, |ui| {

    ui.heading("Settings");
    ui.separator();

    // Source type
    ui.label("Active Source:");
    egui::ComboBox::from_id_salt("source_type")
        .selected_text(settings.active_source.label())
        .show_ui(ui, |ui| {
            for &src in SourceType::ALL {
                ui.selectable_value(&mut settings.active_source, src, src.label());
            }
        });

    ui.add_space(4.0);

    ui.label("Viewer Mode:");
    setting_widgets::viewer_mode_combo(ui, "settings_viewer_mode", &mut settings.viewer_mode);

    ui.add_space(4.0);

    ui.horizontal(|ui| {
        ui.label("Filmstrip before:");
        ui.add(
            egui::DragValue::new(&mut settings.filmstrip_before_count)
                .range(0..=20)
                .speed(0.1),
        );
        ui.label("  after:");
        ui.add(
            egui::DragValue::new(&mut settings.filmstrip_after_count)
                .range(0..=20)
                .speed(0.1),
        );
    });

    ui.horizontal(|ui| {
        ui.label("Center frame size:");
        ui.add(
            egui::DragValue::new(&mut settings.filmstrip_center_percent)
                .range(110..=500)
                .speed(1)
                .suffix("%"),
        );
    });

    ui.add_space(4.0);

    ui.label("Time Display:");
    setting_widgets::time_format_combo(ui, "settings_time_format", &mut settings.time_format);

    ui.add_space(8.0);
    ui.separator();

    // Source-specific settings
    match settings.active_source {
        SourceType::Http => {
            ui.label("HTTP Server URL:");
            ui.text_edit_singleline(&mut settings.http_url);
        }
        SourceType::Folder => {
            folder_field(ui, "Local Folder Path:", &mut settings.folder_path);
        }
        SourceType::Adb => {
            ui.label("Device Path:");
            ui.text_edit_singleline(&mut settings.adb_device_path);
        }
    }

    ui.add_space(8.0);
    ui.separator();
    ui.label("Tool Paths:");

    file_field(ui, "FFmpeg:", &mut settings.ffmpeg_path);
    file_field(ui, "ADB:", &mut settings.adb_path);

    ui.add_space(8.0);
    ui.separator();
    ui.label("Storage Paths:");

    folder_field(ui, "Videos folder:", &mut settings.completed_folder);
    folder_field(ui, "Import temp folder:", &mut settings.temp_folder);
    folder_field(ui, "Frame cache folder:", &mut settings.cache_folder);

    ui.add_space(8.0);
    ui.separator();
    ui.label("Cache Limits:");

    ui.horizontal(|ui| {
        ui.label("Disk cache (MB):");
        ui.add(egui::DragValue::new(&mut settings.disk_cache_mb).range(20480..=102400).speed(100));
    });
    ui.horizontal(|ui| {
        ui.label("GPU cache (MB):");
        ui.add(egui::DragValue::new(&mut settings.gpu_cache_mb).range(64..=16384).speed(50));
    });

    ui.add_space(8.0);
    ui.separator();
    ui.label("Compression:");

    // Algorithm dropdown
    ui.horizontal(|ui| {
        ui.label("Algorithm:");
        egui::ComboBox::from_id_salt("compression_algo")
            .selected_text(settings.compression_algorithm.label())
            .show_ui(ui, |ui| {
                for &algo in CompressionAlgorithm::ALL {
                    ui.selectable_value(&mut settings.compression_algorithm, algo, algo.label());
                }
            });
    });

    // Auto-clamp level when algorithm changes
    if !settings.compression_algorithm.levels().contains(&settings.compression_level) {
        settings.compression_level = settings.compression_algorithm.default_level();
    }

    // Force CompressedOnly when GPU-compressed algorithm is selected
    // (BC has no raw RGBA representation, so Both and RawOnly are invalid)
    if settings.compression_algorithm.is_gpu_compressed()
        && settings.cache_storage_mode != CacheStorageMode::CompressedOnly
    {
        settings.cache_storage_mode = CacheStorageMode::CompressedOnly;
    }

    // Compression level dropdown (algorithm-specific)
    ui.horizontal(|ui| {
        ui.label("Compression Level:");
        let levels = settings.compression_algorithm.levels();
        let level_count = *levels.end() - *levels.start() + 1;
        if level_count <= 1 {
            ui.label("Default");
        } else {
            egui::ComboBox::from_id_salt("compression_level")
                .selected_text(settings.compression_algorithm.level_label(settings.compression_level))
                .show_ui(ui, |ui| {
                    for lvl in levels {
                        ui.selectable_value(
                            &mut settings.compression_level,
                            lvl,
                            settings.compression_algorithm.level_label(lvl),
                        );
                    }
                });
        }
    });

    // Compression device dropdown (only relevant for BC1/BC7)
    let is_bc = settings.compression_algorithm.is_gpu_compressed();
    ui.horizontal(|ui| {
        ui.label("Compression Device:");
        if is_bc {
            // Auto-fallback: if selected device is unavailable, resolve to best
            if !gpu_availability.is_available(settings.compression_device) {
                settings.compression_device = gpu_availability.best_fallback(settings.compression_device);
            }
            egui::ComboBox::from_id_salt("compression_device")
                .selected_text(settings.compression_device.label())
                .show_ui(ui, |ui| {
                    for &device in CompressionDevice::ALL {
                        let available = gpu_availability.is_available(device);
                        ui.add_enabled_ui(available, |ui| {
                            let resp = ui.selectable_value(
                                &mut settings.compression_device,
                                device,
                                device.label(),
                            );
                            if !available {
                                resp.on_disabled_hover_text("No compatible adapter detected");
                            }
                        });
                    }
                });
        } else {
            ui.colored_label(
                egui::Color32::from_gray(140),
                format!("(N/A for {})", settings.compression_algorithm.label()),
            );
        }
    });

    // Warnings for BC compression
    if settings.compression_algorithm.is_gpu_compressed() {
        ui.label(
            egui::RichText::new("BC1/BC7 are lossy compressions.")
                .small()
                .color(egui::Color32::from_gray(140)),
        );
        if settings.compression_algorithm == CompressionAlgorithm::Bc7 {
            match settings.compression_level {
                3 | 4 => {
                    ui.label(
                        egui::RichText::new("This quality level is extremely slow!")
                            .small()
                            .color(egui::Color32::from_rgb(220, 60, 60)),
                    );
                }
                2 => {
                    ui.label(
                        egui::RichText::new("This quality level is very slow.")
                            .small()
                            .color(egui::Color32::from_rgb(220, 180, 40)),
                    );
                }
                _ => {}
            }
        }
    }

    // Storage mode dropdown
    ui.horizontal(|ui| {
        ui.label("Cache Storage Mode:");
        egui::ComboBox::from_id_salt("cache_storage_mode")
            .selected_text(settings.cache_storage_mode.label())
            .show_ui(ui, |ui| {
                for &mode in CacheStorageMode::ALL {
                    let enabled = !(is_bc && mode != CacheStorageMode::CompressedOnly);
                    ui.add_enabled_ui(enabled, |ui| {
                        let resp = ui.selectable_value(&mut settings.cache_storage_mode, mode, mode.label());
                        if !enabled {
                            resp.on_disabled_hover_text("BC1/BC7 textures are GPU-native and have no raw format");
                        }
                    });
                }
            });
    });

    ui.add_space(8.0);
    ui.separator();
    ui.label("Benchmark:");

    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt("benchmark_mode")
            .selected_text(validation.selected_benchmark_mode.label())
            .show_ui(ui, |ui| {
                for &mode in BenchmarkMode::ALL {
                    ui.selectable_value(&mut validation.selected_benchmark_mode, mode, mode.label());
                }
            });
        if ui.button("Run").clicked() {
            action = SettingsAction::Benchmark(validation.selected_benchmark_mode);
        }
    });

    let desc = match validation.selected_benchmark_mode {
        BenchmarkMode::CurrentSettings => "Tests the saved compression settings on the first recording.",
        BenchmarkMode::SmallSubset => "Tests LZ4, PNG 0/3, Zstd 1/3, BC1, BC7. GPU adds extra BC7 levels.",
        BenchmarkMode::All => "Tests every algorithm and level (39 runs).",
    };
    ui.label(
        egui::RichText::new(desc)
            .small()
            .color(egui::Color32::from_gray(140)),
    );

    ui.add_space(16.0);
    ui.separator();

    let mut validated = false;

    if validation.showing_validation {
        ui.colored_label(
            egui::Color32::from_rgb(220, 160, 40),
            "Settings issues detected:",
        );
        for error in validation.errors.clone() {
            ui.label(format!("  \u{2022} {}", error));
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.button("Save Anyway").clicked() {
                validation.showing_validation = false;
                validation.errors.clear();
                validated = true;
            }
            if ui.button("Go Back").clicked() {
                validation.showing_validation = false;
                validation.errors.clear();
            }
        });
    } else if validation.showing_moves {
        ui.colored_label(
            egui::Color32::from_rgb(220, 160, 40),
            "Folder paths changed:",
        );
        for mv in &validation.pending_moves {
            if mv.has_files {
                ui.label(format!(
                    "  \u{2022} {}: files will be moved from '{}' to '{}'",
                    mv.label,
                    mv.old_path.display(),
                    mv.new_path.display(),
                ));
            }
        }
        ui.add_space(2.0);
        ui.label("If moving fails, files may remain in the old location.");
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.button("Proceed").clicked() {
                validation.showing_moves = false;
                action = SettingsAction::Save;
            }
            if ui.button("Cancel").clicked() {
                validation.showing_moves = false;
                validation.pending_moves.clear();
            }
        });
    } else if ui.button("Save Settings").clicked() {
        let errors = settings.validate(exe_dir);
        if errors.is_empty() {
            validated = true;
        } else {
            validation.errors = errors;
            validation.showing_validation = true;
        }
    }

    // After validation passes, check for folder moves
    if validated {
        let moves = compute_folder_moves(old_settings, settings, exe_dir);
        if moves.iter().any(|m| m.has_files) {
            validation.pending_moves = moves;
            validation.showing_moves = true;
        } else {
            validation.pending_moves = moves;
            action = SettingsAction::Save;
        }
    }

    }); // Frame
    }); // ScrollArea

    action
}

fn folder_field(ui: &mut egui::Ui, label: &str, value: &mut String) {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.text_edit_singleline(value);
        if ui.button("Browse").clicked() {
            let mut dialog = rfd::FileDialog::new();
            let dir = Path::new(value.as_str());
            if dir.is_dir() {
                dialog = dialog.set_directory(dir);
            }
            if let Some(path) = dialog.pick_folder() {
                *value = path.to_string_lossy().into_owned();
            }
        }
    });
}

fn file_field(ui: &mut egui::Ui, label: &str, value: &mut String) {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.text_edit_singleline(value);
        if ui.button("Browse").clicked() {
            let mut dialog = rfd::FileDialog::new();
            let p = Path::new(value.as_str());
            if let Some(parent) = p.parent() {
                if parent.is_dir() {
                    dialog = dialog.set_directory(parent);
                }
            }
            if let Some(path) = dialog.pick_file() {
                *value = path.to_string_lossy().into_owned();
            }
        }
    });
}

/// Detect which storage folders changed and whether the old directories contain files.
fn compute_folder_moves(
    old_settings: &Settings,
    new_settings: &Settings,
    exe_dir: &Path,
) -> Vec<PendingFolderMove> {
    let mut moves = Vec::new();

    let checks: [(&str, PathBuf, PathBuf); 3] = [
        (
            "Videos folder",
            old_settings.completed_dir(exe_dir),
            new_settings.completed_dir(exe_dir),
        ),
        (
            "Import temp folder",
            old_settings.temp_dir(exe_dir),
            new_settings.temp_dir(exe_dir),
        ),
        (
            "Frame cache folder",
            old_settings.cache_dir(exe_dir),
            new_settings.cache_dir(exe_dir),
        ),
    ];

    for (label, old_path, new_path) in checks {
        if old_path == new_path {
            continue;
        }
        // Handle case-insensitive / canonical equivalence
        if let (Ok(a), Ok(b)) = (old_path.canonicalize(), new_path.canonicalize()) {
            if a == b {
                continue;
            }
        }

        let has_files = old_path.exists()
            && std::fs::read_dir(&old_path)
                .map(|mut entries| entries.next().is_some())
                .unwrap_or(false);

        moves.push(PendingFolderMove {
            label: label.to_string(),
            old_path,
            new_path,
            has_files,
        });
    }

    moves
}

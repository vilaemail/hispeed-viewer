use crate::platform::PerfMonitor;
use crate::settings::{Settings, SourceType, VideoSettings};
use crate::source::traits::*;
use crate::source::{adb::AdbSource, folder::FolderSource, http::HttpSource};
use crate::sync::file_sync::{FileSync, SyncedFile};
use crate::ui::offset_window::{OffsetAction, SetOffsetState};
use crate::ui::setting_widgets::{OffsetInlineAction, OffsetInlineState};
use crate::ui::settings_window::{SettingsAction, SettingsValidationState};
use crate::ui::sync_status::PendingChange as UiPendingChange;
use crate::video::cache::FrameCache;
use crate::video::extractor::{ExtractionProgress, FrameExtractor};
use crate::video::metadata::VideoMetadata;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Instant;

pub struct App {
    // Core state
    settings: Settings,
    settings_path: PathBuf,
    exe_dir: PathBuf,

    // Video state
    frame_cache: FrameCache,
    current_frame: u32,
    total_frames: u32,
    current_metadata: Option<VideoMetadata>,
    current_recording_index: Option<usize>,

    // Time overlay position
    overlay_pos: egui::Pos2,

    // Frame extraction
    extraction_rx: Option<mpsc::Receiver<ExtractionProgress>>,
    extraction_cancel: Option<Arc<AtomicBool>>,
    extraction_progress: Option<ExtractionProgress>,

    // Background lz4→raw decompression
    decompress_cancel: Option<Arc<AtomicBool>>,
    decompress_handle: Option<std::thread::JoinHandle<()>>,

    // File sync
    file_sync: Option<FileSync>,
    recordings: Vec<SyncedFile>,

    // Performance monitor
    perf_monitor: PerfMonitor,

    // Per-video offset (loaded from filename.settings.json)
    time_offset_seconds: f64,
    current_video_path: Option<PathBuf>,

    // UI state
    show_settings: bool,
    show_sync_status: bool,
    show_set_offset: bool,
    settings_draft: Settings,
    settings_validation: SettingsValidationState,
    settings_errors: Vec<String>,
    error_banner: Option<String>,
    warning_banner: Option<String>,
    timeline_state: crate::ui::timeline::TimelineState,
    set_offset_state: SetOffsetState,
    offset_inline_state: OffsetInlineState,
    viewer_zoom: crate::ui::viewer::ViewerZoomState,
    show_shortcuts: bool,
    show_about: bool,

    // Playback
    is_playing: bool,
    playback_fps: f64,
    frame_accumulator: f64,
    last_play_instant: Option<Instant>,
    playback_speed_state: crate::ui::viewer_toolbar::PlaybackSpeedState,
}

impl App {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let exe_dir = crate::platform::exe_dir();
        let settings_path = exe_dir.join("settings.json");
        let settings = Settings::load(&settings_path);

        // Ensure directories exist
        let completed_dir = settings.completed_dir(&exe_dir);
        let temp_dir = settings.temp_dir(&exe_dir);
        let cache_dir = settings.cache_dir(&exe_dir);
        let _ = std::fs::create_dir_all(&completed_dir);
        let _ = std::fs::create_dir_all(&temp_dir);
        let _ = std::fs::create_dir_all(&cache_dir);

        // Validate settings on boot
        let settings_errors = settings.validate(&exe_dir);
        if !settings_errors.is_empty() {
            for e in &settings_errors {
                log::warn!("Settings issue: {e}");
            }
        }

        // Start file sync
        let source = create_source(&settings, &exe_dir);
        let file_sync = FileSync::new(source, temp_dir, completed_dir);

        let settings_draft = settings.clone();
        let overlay_pos = egui::pos2(settings.time_overlay_x, settings.time_overlay_y);

        let mut perf_monitor = PerfMonitor::new();
        perf_monitor.set_disk_paths(
            &settings.completed_dir(&exe_dir),
            &settings.cache_dir(&exe_dir),
            settings.disk_cache_mb,
            settings.gpu_cache_mb,
        );

        Self {
            settings,
            settings_path,
            exe_dir,
            frame_cache: FrameCache::new(),
            current_frame: 0,
            total_frames: 0,
            current_metadata: None,
            current_recording_index: None,
            overlay_pos,
            extraction_rx: None,
            extraction_cancel: None,
            extraction_progress: None,
            decompress_cancel: None,
            decompress_handle: None,
            file_sync: Some(file_sync),
            recordings: Vec::new(),
            perf_monitor,
            time_offset_seconds: 0.0,
            current_video_path: None,
            show_settings: false,
            show_sync_status: false,
            show_set_offset: false,
            settings_draft,
            settings_validation: SettingsValidationState::default(),
            settings_errors,
            error_banner: None,
            warning_banner: None,
            timeline_state: crate::ui::timeline::TimelineState::default(),
            set_offset_state: SetOffsetState::default(),
            offset_inline_state: OffsetInlineState::default(),
            viewer_zoom: crate::ui::viewer::ViewerZoomState::default(),
            show_shortcuts: false,
            show_about: false,
            is_playing: false,
            playback_fps: 30.0,
            frame_accumulator: 0.0,
            last_play_instant: None,
            playback_speed_state: crate::ui::viewer_toolbar::PlaybackSpeedState::new(30.0),
        }
    }

    fn open_recording(&mut self, synced: &SyncedFile, _ctx: &egui::Context) {
        // Close any existing recording
        self.close_recording();

        let metadata = synced.metadata.clone();
        let cache_dir = self
            .settings
            .cache_dir(&self.exe_dir)
            .join(&metadata.sha256);

        self.total_frames = metadata.total_frames;
        self.current_frame = 0;

        // Check if frames are already extracted
        if let Some(info) = FrameExtractor::is_already_extracted(&cache_dir) {
            log::info!("Frames already extracted for {}", synced.json_name);
            crate::video::cache::touch_cache_access(&cache_dir);
            // Use actual dimensions from extraction (may differ from metadata)
            self.frame_cache.set_gpu_budget(self.settings.gpu_cache_mb, info.frame_width, info.frame_height);
            self.frame_cache.set_source(&cache_dir, metadata.total_frames, info.frame_width, info.frame_height);
            self.frame_cache.set_current_frame(0);
            // Warn if cached frame count doesn't match expected
            if info.frame_count != metadata.total_frames {
                let msg = format!(
                    "Frame count mismatch: MP4 produced {} frames but JSON expects {}",
                    info.frame_count, metadata.total_frames
                );
                log::warn!("{msg}");
                self.warning_banner = Some(msg);
            }
            // Start background decompression if raw cache is missing
            if !FrameExtractor::has_raw_cache(&cache_dir) {
                self.start_decompress(&cache_dir);
            }
        } else {
            // Use metadata dimensions for GPU budget (will be updated after extraction)
            self.frame_cache.set_gpu_budget(self.settings.gpu_cache_mb, metadata.width, metadata.height);

            // Proactively evict caches to make room for this video
            let estimated = crate::video::cache::estimate_cache_bytes(
                metadata.total_frames, metadata.width, metadata.height,
            );
            let cache_root = self.settings.cache_dir(&self.exe_dir);
            crate::video::cache::evict_for_new_video(
                &cache_root,
                self.settings.disk_cache_mb,
                &metadata.sha256,
                estimated,
            );

            // Start extraction
            log::info!("Starting frame extraction for {}", synced.json_name);
            let extractor = FrameExtractor::new(&self.settings.ffmpeg_path);
            let (rx, cancel) = extractor.extract_async(
                synced.video_path.clone(),
                cache_dir.clone(),
                metadata.total_frames,
                metadata.width,
                metadata.height,
            );
            self.extraction_rx = Some(rx);
            self.extraction_cancel = Some(cancel);
            self.extraction_progress = Some(ExtractionProgress {
                total_frames: metadata.total_frames,
                extracted_frames: 0,
                complete: false,
                failed: false,
                error_message: String::new(),
                warning_message: String::new(),
            });
        }

        // Load per-video settings (offset, overlay position, etc.)
        let video_settings = VideoSettings::load(&synced.video_path);
        self.time_offset_seconds = video_settings.time_offset_seconds;
        self.overlay_pos = egui::pos2(video_settings.overlay_x, video_settings.overlay_y);
        self.current_video_path = Some(synced.video_path.clone());

        self.current_metadata = Some(metadata.clone());

        // Sync playback speed fields with the new video's original FPS.
        self.playback_speed_state
            .sync_from_fps(self.playback_fps, Some(metadata.fps));
    }

    fn start_decompress(&mut self, cache_dir: &Path) {
        // Cancel any existing decompression
        if let Some(cancel) = &self.decompress_cancel {
            cancel.store(true, Ordering::Relaxed);
        }
        if let Some(handle) = self.decompress_handle.take() {
            let _ = handle.join();
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let handle = crate::video::cache::decompress_cache_async(
            cache_dir.to_path_buf(),
            cancel.clone(),
        );
        self.decompress_cancel = Some(cancel);
        self.decompress_handle = Some(handle);
    }

    fn close_recording(&mut self) {
        // Cancel any ongoing extraction
        if let Some(cancel) = &self.extraction_cancel {
            cancel.store(true, Ordering::Relaxed);
        }
        self.extraction_rx = None;
        self.extraction_cancel = None;
        self.extraction_progress = None;

        // Cancel any ongoing decompression
        if let Some(cancel) = &self.decompress_cancel {
            cancel.store(true, Ordering::Relaxed);
        }
        // Don't join here to avoid blocking UI; thread will exit on cancel
        self.decompress_cancel = None;
        self.decompress_handle = None;

        self.frame_cache.clear();
        self.current_metadata = None;
        self.current_frame = 0;
        self.total_frames = 0;
        self.current_recording_index = None;
        self.time_offset_seconds = 0.0;
        self.current_video_path = None;
        self.viewer_zoom.reset();
        self.is_playing = false;
        self.last_play_instant = None;
        self.frame_accumulator = 0.0;
    }

    fn update_extraction(&mut self) {
        let rx = match &self.extraction_rx {
            Some(rx) => rx,
            None => return,
        };

        // Drain all available progress updates
        let mut latest = None;
        while let Ok(progress) = rx.try_recv() {
            latest = Some(progress);
        }

        if let Some(progress) = latest {
            if progress.complete {
                if progress.failed {
                    log::error!("Extraction failed: {}", progress.error_message);
                    // Remove partial/invalid cache directory
                    if let Some(meta) = &self.current_metadata {
                        let cache_dir = self
                            .settings
                            .cache_dir(&self.exe_dir)
                            .join(&meta.sha256);
                        if cache_dir.is_dir() {
                            let _ = std::fs::remove_dir_all(&cache_dir);
                            log::info!("Removed invalid cache at {}", cache_dir.display());
                        }
                    }
                    self.error_banner = Some(progress.error_message.clone());
                } else {
                    // Show warning if frame count mismatched (but still load)
                    if !progress.warning_message.is_empty() {
                        log::warn!("{}", progress.warning_message);
                        self.warning_banner = Some(progress.warning_message.clone());
                    }

                    // Extraction done - set up frame cache
                    if let Some(meta) = &self.current_metadata {
                        let sha = meta.sha256.clone();
                        let total = meta.total_frames;
                        let (mw, mh) = (meta.width, meta.height);
                        let cache_dir = self.settings.cache_dir(&self.exe_dir).join(&sha);
                        crate::video::cache::touch_cache_access(&cache_dir);
                        // Use actual dimensions from extraction marker
                        let (fw, fh) = FrameExtractor::is_already_extracted(&cache_dir)
                            .map(|info| (info.frame_width, info.frame_height))
                            .unwrap_or((mw, mh));
                        self.frame_cache.set_gpu_budget(self.settings.gpu_cache_mb, fw, fh);
                        self.frame_cache.set_source(&cache_dir, total, fw, fh);
                        self.frame_cache.set_current_frame(self.current_frame);

                        // Start background decompression (lz4 → raw)
                        self.start_decompress(&cache_dir);

                        // Enforce disk cache limit
                        let cache_root = self.settings.cache_dir(&self.exe_dir);
                        if let Err(e) = crate::video::cache::manage_disk_cache(
                            &cache_root,
                            self.settings.disk_cache_mb,
                            &sha,
                        ) {
                            log::error!("{e}");
                            self.error_banner = Some(e);
                        }
                    }
                }
                self.extraction_rx = None;
                self.extraction_cancel = None;
                self.extraction_progress = None;
            } else {
                self.extraction_progress = Some(progress);
            }
        }
    }

    fn toggle_playback(&mut self) {
        if self.total_frames == 0 {
            return;
        }
        self.is_playing = !self.is_playing;
        if self.is_playing {
            // If we're at the last frame, wrap to start.
            if self.current_frame >= self.total_frames.saturating_sub(1) {
                self.current_frame = 0;
                self.frame_cache.set_current_frame(0);
            }
            self.last_play_instant = Some(Instant::now());
            self.frame_accumulator = 0.0;
        } else {
            self.last_play_instant = None;
            self.frame_accumulator = 0.0;
        }
    }

    fn advance_playback(&mut self, ctx: &egui::Context) {
        if !self.is_playing || self.total_frames == 0 {
            return;
        }

        let now = Instant::now();
        if let Some(prev) = self.last_play_instant {
            let elapsed = now.duration_since(prev).as_secs_f64();
            self.frame_accumulator += elapsed * self.playback_fps;
        }
        self.last_play_instant = Some(now);

        let last_frame = self.total_frames - 1;
        let frames_to_advance = self.frame_accumulator as u32;
        if frames_to_advance > 0 {
            self.frame_accumulator -= frames_to_advance as f64;
            let new = (self.current_frame + frames_to_advance).min(last_frame);
            if new != self.current_frame {
                self.current_frame = new;
                self.frame_cache.set_current_frame(new);
            }
            if self.current_frame >= last_frame {
                self.is_playing = false;
                self.last_play_instant = None;
                self.frame_accumulator = 0.0;
            }
        }

        if self.is_playing {
            ctx.request_repaint();
        }
    }

    fn handle_input(&mut self, ctx: &egui::Context) {
        if self.total_frames == 0 {
            return;
        }

        // Spacebar toggle (only when no text field is focused)
        let space_pressed = ctx.input(|input| input.key_pressed(egui::Key::Space));
        if space_pressed && !ctx.wants_keyboard_input() {
            self.toggle_playback();
            // Sync speed state text for the newly loaded metadata (if any).
            let original_fps = self.current_metadata.as_ref().map(|m| m.fps);
            self.playback_speed_state
                .sync_from_fps(self.playback_fps, original_fps);
        }

        // Skip manual frame-step keys while playing.
        if self.is_playing || ctx.wants_keyboard_input() {
            return;
        }

        let mut new_frame = self.current_frame as i32;

        ctx.input(|input| {
            if input.key_pressed(egui::Key::ArrowLeft) {
                new_frame -= 1;
            }
            if input.key_pressed(egui::Key::ArrowRight) {
                new_frame += 1;
            }
            if input.key_pressed(egui::Key::Home) {
                new_frame = 0;
            }
            if input.key_pressed(egui::Key::End) {
                new_frame = self.total_frames as i32 - 1;
            }
            if input.key_pressed(egui::Key::PageUp) {
                new_frame -= 30;
            }
            if input.key_pressed(egui::Key::PageDown) {
                new_frame += 30;
            }
            // Mouse wheel on the viewer area (skip when Ctrl/Shift held — used for zoom/center size)
            if !input.modifiers.ctrl && !input.modifiers.shift {
                let scroll = input.raw_scroll_delta.y;
                if scroll.abs() > 0.1 {
                    new_frame += if scroll > 0.0 { -1 } else { 1 };
                }
            }
        });

        let clamped = new_frame.clamp(0, self.total_frames as i32 - 1) as u32;
        if clamped != self.current_frame {
            self.current_frame = clamped;
            self.frame_cache.set_current_frame(clamped);
        }
    }

    /// Save global settings to disk.
    fn save_settings(&mut self) {
        self.settings.time_overlay_x = self.overlay_pos.x;
        self.settings.time_overlay_y = self.overlay_pos.y;
        if let Err(e) = self.settings.save(&self.settings_path) {
            log::error!("Failed to save settings: {e}");
        }
    }

    /// Build the current per-video settings struct.
    fn current_video_settings(&self) -> VideoSettings {
        VideoSettings {
            time_offset_seconds: self.time_offset_seconds,
            overlay_x: self.overlay_pos.x,
            overlay_y: self.overlay_pos.y,
        }
    }

    /// Save the current video's offset to its per-file settings.
    fn save_video_offset(&self) {
        if let Some(vpath) = &self.current_video_path {
            if let Err(e) = self.current_video_settings().save(vpath) {
                log::error!("Failed to save video settings: {e}");
            }
        }
    }

    /// Save the current video's overlay position to its per-file settings.
    fn save_video_overlay(&self) {
        if let Some(vpath) = &self.current_video_path {
            if let Err(e) = self.current_video_settings().save(vpath) {
                log::error!("Failed to save video settings: {e}");
            }
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Tick perf monitor
        self.perf_monitor.tick();

        // Poll file sync
        if let Some(sync) = &mut self.file_sync {
            sync.poll();
            self.recordings = sync.completed_files.clone();
        }

        // Update extraction progress
        self.update_extraction();

        // Advance playback (must be before handle_input so spacebar toggle is clean)
        self.advance_playback(ctx);

        // Handle keyboard/mouse input before uploads so new frame position is known
        self.handle_input(ctx);

        // Process frame cache uploads (time-budgeted to keep UI responsive)
        let has_pending = self.frame_cache.process_uploads(ctx, 4.0);

        // Request repaint if we have active work
        if has_pending || self.extraction_progress.is_some() || self.extraction_rx.is_some() || self.is_playing {
            // Active uploads or extraction: repaint immediately for smooth progress
            ctx.request_repaint();
        } else if self.frame_cache.is_prefetch_active() {
            // Prefetch running but no frames to upload right now:
            // check periodically (60fps) instead of spinning at max FPS
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
        }

        // --- UI Rendering ---

        // Top menu bar
        egui::TopBottomPanel::top("menu_bar").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Set Offset").clicked() {
                        self.show_set_offset = !self.show_set_offset;
                        ui.close_menu();
                    }
                    if ui.button("Settings").clicked() {
                        self.show_settings = !self.show_settings;
                        self.settings_draft = self.settings.clone();
                        self.settings_draft.resolve_all_paths(&self.exe_dir);
                        self.settings_validation = SettingsValidationState::default();
                        ui.close_menu();
                    }
                    if ui.button("Sync Now").clicked() {
                        if let Some(sync) = &self.file_sync {
                            sync.trigger_sync();
                        }
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("Quit").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });

                ui.menu_button("View", |ui| {
                    if ui.button("Sync Status").clicked() {
                        self.show_sync_status = !self.show_sync_status;
                        ui.close_menu();
                    }
                });

                ui.menu_button("Help", |ui| {
                    if ui.button("Shortcuts").clicked() {
                        self.show_shortcuts = !self.show_shortcuts;
                        ui.close_menu();
                    }
                    if ui.button("About").clicked() {
                        self.show_about = !self.show_about;
                        ui.close_menu();
                    }
                });

                // Right side: perf stats + sync status
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // Sync indicator
                    if let Some(sync) = &self.file_sync {
                        let (color, text) = match sync.source_status {
                            SourceStatus::Available => {
                                (egui::Color32::from_rgb(60, 180, 60), "Connected")
                            }
                            SourceStatus::Unavailable => {
                                (egui::Color32::from_rgb(180, 60, 60), "Disconnected")
                            }
                            SourceStatus::Checking => {
                                (egui::Color32::from_rgb(180, 180, 60), "Checking")
                            }
                        };
                        let (r, _) =
                            ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                        ui.painter().circle_filled(r.center(), 4.0, color);
                        ui.label(text);
                    }

                    ui.separator();

                    // Performance stats with progress bars
                    let perf = self.perf_monitor.perf().clone();
                    let mono = |s: String| {
                        egui::RichText::new(s).monospace().size(15.0).color(egui::Color32::from_gray(160))
                    };
                    let sep = || egui::RichText::new(" | ").monospace().size(15.0).color(egui::Color32::from_gray(80));

                    let blue = egui::Color32::from_rgb(60, 100, 200);
                    let green = egui::Color32::from_rgb(60, 160, 60);
                    let yellow = egui::Color32::from_rgb(200, 180, 40);

                    // Cache disk
                    if let Some((used_mb, limit_mb)) = perf.cache_disk {
                        let frac = if limit_mb > 0.0 { used_mb / limit_mb } else { 0.0 };
                        let used_gb = used_mb as f64 / 1024.0;
                        let limit_gb = limit_mb as f64 / 1024.0;
                        ui.label(mono(format!("Cache: {:.2}/{:.2} GB", used_gb, limit_gb)));
                        perf_bar(ui, frac, 40.0, blue);
                    }

                    ui.label(sep());

                    // Video disk: folder size vs free space on drive
                    if let Some((folder_bytes, free_bytes)) = perf.video_disk {
                        let folder_gb = folder_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
                        let free_gb = free_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
                        let frac = if free_bytes > 0 { folder_bytes as f32 / free_bytes as f32 } else { 0.0 };
                        ui.label(mono(format!("DISK: {:.2} / {:.2} GB", folder_gb, free_gb)));
                        perf_bar(ui, frac, 40.0, blue);
                    }

                    ui.label(sep());

                    // GPU Memory
                    if let (Some(used), Some(total)) = (perf.gpu_memory_mb, perf.gpu_total_mb) {
                        let frac = if total > 0.0 { used / total } else { 0.0 };
                        let used_gb = used as f64 / 1024.0;
                        let total_gb = total as f64 / 1024.0;
                        ui.label(mono(format!("GPU MEMORY: {:.2}/{:.2} GB", used_gb, total_gb)));
                        perf_bar(ui, frac, 40.0, green);
                    } else if let Some(vram) = perf.gpu_memory_mb {
                        let vram_gb = vram as f64 / 1024.0;
                        ui.label(mono(format!("GPU MEMORY: {:.2} GB", vram_gb)));
                    }

                    ui.label(sep());

                    // Memory: our working set vs (our usage + system available)
                    {
                        let frac = if perf.total_memory_mb > 0.0 {
                            perf.memory_mb / perf.total_memory_mb
                        } else {
                            0.0
                        };
                        let mem_gb = perf.memory_mb as f64 / 1024.0;
                        let total_gb = perf.total_memory_mb as f64 / 1024.0;
                        ui.label(mono(format!("MEMORY: {:.2}/{:.2} GB", mem_gb, total_gb)));
                        perf_bar(ui, frac, 40.0, yellow);
                    }

                    ui.label(sep());

                    ui.label(mono(format!("CPU: {:.0}%  FPS: {:.0}", perf.cpu_percent, perf.fps)));
                });
            });
        });

        // Settings validation errors
        let mut dismiss_errors = false;
        let mut open_settings_from_errors = false;
        if !self.settings_errors.is_empty() {
            egui::TopBottomPanel::top("settings_errors").show(ctx, |ui| {
                ui.colored_label(
                    egui::Color32::from_rgb(220, 160, 40),
                    "Settings issues detected:",
                );
                for error in &self.settings_errors {
                    ui.label(format!("  \u{2022} {}", error));
                }
                ui.horizontal(|ui| {
                    if ui.small_button("Dismiss").clicked() {
                        dismiss_errors = true;
                    }
                    if ui.small_button("Open Settings").clicked() {
                        open_settings_from_errors = true;
                    }
                });
            });
        }
        if dismiss_errors {
            self.settings_errors.clear();
        }
        if open_settings_from_errors {
            self.show_settings = true;
            self.settings_draft = self.settings.clone();
            self.settings_draft.resolve_all_paths(&self.exe_dir);
            self.settings_validation = SettingsValidationState::default();
            self.settings_errors.clear();
        }

        // General error banner
        let mut dismiss_banner = false;
        if let Some(error) = &self.error_banner {
            egui::TopBottomPanel::top("error_banner").show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.colored_label(egui::Color32::from_rgb(220, 60, 60), error);
                    if ui.small_button("Dismiss").clicked() {
                        dismiss_banner = true;
                    }
                });
            });
        }
        if dismiss_banner {
            self.error_banner = None;
        }

        // Warning banner (yellow, for non-fatal issues like frame count mismatch)
        let mut dismiss_warning = false;
        if let Some(warning) = &self.warning_banner {
            egui::TopBottomPanel::top("warning_banner").show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.colored_label(egui::Color32::from_rgb(220, 160, 40), warning);
                    if ui.small_button("Dismiss").clicked() {
                        dismiss_warning = true;
                    }
                });
            });
        }
        if dismiss_warning {
            self.warning_banner = None;
        }

        // Bottom timeline panel (resizable)
        egui::TopBottomPanel::bottom("timeline_panel")
            .resizable(true)
            .default_height(80.0)
            .min_height(48.0)
            .show(ctx, |ui| {
                let cache = &self.frame_cache;
                let offset = self.time_offset_seconds;
                if let Some(new_frame) = crate::ui::timeline::render_timeline(
                    ui,
                    self.current_frame,
                    self.total_frames,
                    self.current_metadata.as_ref(),
                    &|idx| cache.is_cached(idx),
                    self.settings.time_format,
                    &mut self.timeline_state,
                    offset,
                ) {
                    self.current_frame = new_frame;
                    self.frame_cache.set_current_frame(new_frame);
                }
            });

        // Left side panel - file browser
        egui::SidePanel::left("file_browser_panel")
            .default_width(200.0)
            .resizable(true)
            .show(ctx, |ui| {
                let browser_recordings: Vec<crate::ui::file_browser::CompletedRecording> = self
                    .recordings
                    .iter()
                    .map(|r| crate::ui::file_browser::CompletedRecording {
                        json_path: r.json_path.clone(),
                        video_path: r.video_path.clone(),
                        display_name: r.json_name.trim_end_matches(".json").to_string(),
                        file_size: r.metadata.size,
                    })
                    .collect();

                if let Some(idx) = crate::ui::file_browser::render_file_browser(
                    ui,
                    &browser_recordings,
                    self.current_recording_index,
                ) {
                    if idx < self.recordings.len() {
                        let rec = self.recordings[idx].clone();
                        self.current_recording_index = Some(idx);
                        self.open_recording(&rec, ctx);
                    }
                }
            });

        // Central panel - frame viewer
        egui::CentralPanel::default().show(ctx, |ui| {
            // Contextual toolbar (only when a recording is loaded)
            if self.current_metadata.is_some() {
                let actions = crate::ui::viewer_toolbar::render_viewer_toolbar(
                    ui,
                    &mut self.settings.viewer_mode,
                    &mut self.settings.time_format,
                    &mut self.offset_inline_state,
                    self.time_offset_seconds,
                    self.current_frame,
                    self.current_metadata.as_ref(),
                    self.is_playing,
                    &mut self.playback_speed_state,
                );

                if actions.playback_toggled {
                    self.toggle_playback();
                }
                if let Some(new_fps) = actions.playback_fps_changed {
                    self.playback_fps = new_fps;
                }
                if actions.time_format_changed {
                    self.settings_draft.time_format = self.settings.time_format;
                    self.save_settings();
                }
                if actions.viewer_mode_changed {
                    self.viewer_zoom.reset();
                    self.settings_draft.viewer_mode = self.settings.viewer_mode;
                    self.save_settings();
                }
                match actions.offset_action {
                    OffsetInlineAction::Applied(new_offset) => {
                        self.time_offset_seconds = new_offset;
                        self.save_video_offset();
                    }
                    OffsetInlineAction::Reset => {
                        self.time_offset_seconds = 0.0;
                        self.save_video_offset();
                    }
                    OffsetInlineAction::None => {}
                }
            }

            let current_texture = self.frame_cache.get_frame(self.current_frame);
            let current_missing = self.frame_cache.is_missing(self.current_frame);

            // Gather neighbor textures for filmstrip mode (only valid frame indices)
            let neighbor_textures: Vec<(u32, Option<egui::TextureHandle>, bool)> =
                if self.settings.viewer_mode == crate::settings::ViewerMode::Filmstrip
                    && self.total_frames > 0
                {
                    let cf = self.current_frame as i64;
                    let total = self.total_frames as i64;
                    let before = self.settings.filmstrip_before_count as i64;
                    let after = self.settings.filmstrip_after_count as i64;
                    let mut textures = Vec::new();
                    for off in 1..=before {
                        let idx = cf - off;
                        if idx >= 0 && idx < total {
                            let idx = idx as u32;
                            textures.push((idx, self.frame_cache.get_frame(idx), self.frame_cache.is_missing(idx)));
                        }
                    }
                    for off in 1..=after {
                        let idx = cf + off;
                        if idx >= 0 && idx < total {
                            let idx = idx as u32;
                            textures.push((idx, self.frame_cache.get_frame(idx), self.frame_cache.is_missing(idx)));
                        }
                    }
                    textures
                } else {
                    Vec::new()
                };

            let frame_dims = {
                let (w, h) = self.frame_cache.frame_dimensions();
                if w > 0 && h > 0 { Some((w, h)) } else { None }
            };
            let viewer_action = crate::ui::viewer::render_viewer(
                ui,
                current_texture.as_ref(),
                current_missing,
                self.current_frame,
                self.total_frames,
                self.current_metadata.as_ref(),
                self.time_offset_seconds,
                &mut self.overlay_pos,
                self.settings.viewer_mode,
                &neighbor_textures,
                self.settings.time_format,
                &mut self.viewer_zoom,
                frame_dims,
                self.settings.filmstrip_before_count,
                self.settings.filmstrip_after_count,
                &mut self.settings.filmstrip_center_percent,
            );
            if viewer_action.overlay_placed {
                self.save_video_overlay();
            }
            if viewer_action.center_percent_changed {
                self.settings_draft.filmstrip_center_percent = self.settings.filmstrip_center_percent;
                self.save_settings();
            }
        });

        // Extraction progress overlay
        if let Some(progress) = &self.extraction_progress {
            if crate::ui::progress::render_extraction_progress(ctx, progress) {
                // User cancelled
                if let Some(cancel) = &self.extraction_cancel {
                    cancel.store(true, Ordering::Relaxed);
                }
            }
        }

        // Settings window
        if self.show_settings {
            egui::Window::new("Settings")
                .open(&mut self.show_settings)
                .resizable(true)
                .default_width(400.0)
                .show(ctx, |ui| {
                    let action = crate::ui::settings_window::render_settings(
                        ui,
                        &mut self.settings_draft,
                        &self.settings,
                        &self.exe_dir,
                        &mut self.settings_validation,
                    );
                    if matches!(action, SettingsAction::Save) {
                        // Perform any pending folder moves before saving
                        for mv in &self.settings_validation.pending_moves {
                            if mv.has_files && mv.old_path.exists() {
                                log::info!(
                                    "Moving {} from '{}' to '{}'",
                                    mv.label,
                                    mv.old_path.display(),
                                    mv.new_path.display()
                                );
                                match move_folder_contents(&mv.old_path, &mv.new_path) {
                                    Ok(_) => {
                                        // Remove old directory on success
                                        if let Err(e) = std::fs::remove_dir_all(&mv.old_path) {
                                            log::warn!(
                                                "Could not remove old dir '{}': {e}",
                                                mv.old_path.display()
                                            );
                                        }
                                    }
                                    Err(e) => {
                                        log::error!("Failed to move {}: {e}", mv.label);
                                        self.error_banner = Some(format!(
                                            "Failed to move {}: {e}. Files may remain in '{}'.",
                                            mv.label,
                                            mv.old_path.display()
                                        ));
                                    }
                                }
                            }
                        }
                        self.settings_validation.pending_moves.clear();
                        self.settings_errors.clear();

                        // Relativize paths before saving
                        self.settings_draft.relativize_all_paths(&self.exe_dir);
                        self.settings = self.settings_draft.clone();
                        // Re-resolve draft so the UI keeps showing absolute paths
                        self.settings_draft.resolve_all_paths(&self.exe_dir);
                        self.settings.time_overlay_x = self.overlay_pos.x;
                        self.settings.time_overlay_y = self.overlay_pos.y;
                        if let Err(e) = self.settings.save(&self.settings_path) {
                            log::error!("Failed to save settings: {e}");
                        } else {
                            log::info!("Settings saved");
                        }

                        // Recompute GPU cache budget if we have a loaded video
                        if self.current_metadata.is_some() {
                            let (fw, fh) = self.frame_cache.frame_dimensions();
                            self.frame_cache.set_gpu_budget(
                                self.settings.gpu_cache_mb,
                                fw,
                                fh,
                            );
                        }

                        // Update perf monitor paths
                        self.perf_monitor.set_disk_paths(
                            &self.settings.completed_dir(&self.exe_dir),
                            &self.settings.cache_dir(&self.exe_dir),
                            self.settings.disk_cache_mb,
                            self.settings.gpu_cache_mb,
                        );

                        // Recreate sync with new settings
                        drop(self.file_sync.take());
                        let source = create_source(&self.settings, &self.exe_dir);
                        let temp_dir = self.settings.temp_dir(&self.exe_dir);
                        let completed_dir = self.settings.completed_dir(&self.exe_dir);
                        let _ = std::fs::create_dir_all(&temp_dir);
                        let _ = std::fs::create_dir_all(&completed_dir);
                        let file_sync = FileSync::new(source, temp_dir, completed_dir);
                        self.file_sync = Some(file_sync);
                    }
                });
        }

        // Set Offset window
        let mut offset_window_action = OffsetAction::None;
        if self.show_set_offset {
            egui::Window::new("Set Offset")
                .open(&mut self.show_set_offset)
                .resizable(false)
                .default_width(340.0)
                .show(ctx, |ui| {
                    offset_window_action = crate::ui::offset_window::render_set_offset(
                        ui,
                        &mut self.set_offset_state,
                        self.current_frame,
                        self.time_offset_seconds,
                        self.current_metadata.as_ref(),
                    );
                });
        }
        match offset_window_action {
            OffsetAction::Applied(offset) => {
                self.time_offset_seconds = offset;
                self.save_video_offset();
            }
            OffsetAction::Reset => {
                self.time_offset_seconds = 0.0;
                self.save_video_offset();
            }
            OffsetAction::None => {}
        }

        // Sync status window
        if self.show_sync_status {
            egui::Window::new("Sync Status")
                .open(&mut self.show_sync_status)
                .show(ctx, |ui| {
                    if let Some(sync) = &self.file_sync {
                        crate::ui::sync_status::render_sync_status(
                            ui,
                            sync.source_status,
                            self.settings.active_source.label(),
                        );
                        ui.separator();
                        ui.label(&sync.status_text);
                        ui.separator();

                        let pending: Vec<UiPendingChange> = sync
                            .pending_changes
                            .iter()
                            .map(|c| UiPendingChange {
                                file_name: c.json_name.clone(),
                                description: c.description.clone(),
                                old_size: format!("{} bytes", c.old_size),
                                new_size: format!("{} bytes", c.new_size),
                            })
                            .collect();

                        let approved = crate::ui::sync_status::render_update_approval(ui, &pending);
                        for idx in approved {
                            if idx < sync.pending_changes.len() {
                                sync.approve_change(&sync.pending_changes[idx].json_name);
                            }
                        }
                    }
                });
        }

        // Shortcuts window
        if self.show_shortcuts {
            egui::Window::new("Keyboard Shortcuts")
                .open(&mut self.show_shortcuts)
                .resizable(false)
                .default_width(380.0)
                .show(ctx, |ui| {
                    render_shortcuts(ui);
                });
        }

        // About window
        if self.show_about {
            egui::Window::new("About")
                .open(&mut self.show_about)
                .resizable(false)
                .default_width(300.0)
                .show(ctx, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(4.0);
                        ui.heading("HiSpeed Viewer");
                        ui.add_space(4.0);
                        ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
                        ui.label(env!("CARGO_PKG_DESCRIPTION"));
                        ui.add_space(8.0);
                        ui.hyperlink_to(
                            "GitHub: vilaemail/hispeed-viewer",
                            "https://github.com/vilaemail/hispeed-viewer",
                        );
                        ui.add_space(4.0);
                    });
                });
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save_settings();

        // Cleanup
        self.close_recording();
        self.file_sync = None;
    }
}

fn create_source(settings: &Settings, exe_dir: &Path) -> Box<dyn RemoteSource> {
    match settings.active_source {
        SourceType::Http => Box::new(HttpSource::new(&settings.http_url)),
        SourceType::Folder => {
            let resolved = settings.resolve_path(exe_dir, &settings.folder_path);
            Box::new(FolderSource::new(&resolved.to_string_lossy()))
        }
        SourceType::Adb => Box::new(AdbSource::new(&settings.adb_path, &settings.adb_device_path)),
    }
}

/// Move all entries from `src` directory into `dst`.
/// Tries rename first (instant on same filesystem), falls back to copy+delete.
/// Skips entries that already exist in `dst`.
fn move_folder_contents(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst)
        .map_err(|e| format!("cannot create '{}': {}", dst.display(), e))?;

    let entries = std::fs::read_dir(src)
        .map_err(|e| format!("cannot read '{}': {}", src.display(), e))?;

    let mut errors = Vec::new();

    for entry in entries.flatten() {
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if dst_path.exists() {
            continue;
        }

        // Try rename first (instant on same filesystem)
        if std::fs::rename(&src_path, &dst_path).is_ok() {
            continue;
        }

        // Fallback: copy then delete (cross-filesystem)
        if src_path.is_dir() {
            if let Err(e) = copy_dir_all(&src_path, &dst_path) {
                errors.push(e);
            } else {
                let _ = std::fs::remove_dir_all(&src_path);
            }
        } else {
            match std::fs::copy(&src_path, &dst_path) {
                Ok(_) => {
                    let _ = std::fs::remove_file(&src_path);
                }
                Err(e) => errors.push(format!("cannot copy '{}': {}", src_path.display(), e)),
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// Recursively copy a directory tree.
fn copy_dir_all(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst)
        .map_err(|e| format!("cannot create '{}': {}", dst.display(), e))?;
    for entry in std::fs::read_dir(src)
        .map_err(|e| format!("cannot read '{}': {}", src.display(), e))?
        .flatten()
    {
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir_all(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)
                .map_err(|e| format!("cannot copy '{}': {}", from.display(), e))?;
        }
    }
    Ok(())
}

fn render_shortcuts(ui: &mut egui::Ui) {
    let shortcuts: &[(&str, &str)] = &[
        ("Arrow Left / Right", "Previous / Next frame"),
        ("Home / End", "First / Last frame"),
        ("Page Up / Page Down", "Skip 30 frames back / forward"),
        ("Space", "Play / Pause"),
        ("Scroll wheel", "Previous / Next frame"),
        ("Ctrl + Scroll", "Zoom in / out"),
        ("Shift + Scroll", "Adjust center frame size (filmstrip)"),
        ("Drag (when zoomed)", "Pan the view"),
        ("Double-click", "Reset zoom"),
        ("Drag overlay", "Move time overlay position"),
    ];

    egui::Grid::new("shortcuts_grid")
        .num_columns(2)
        .spacing([16.0, 4.0])
        .show(ui, |ui| {
            for &(key, desc) in shortcuts {
                ui.label(egui::RichText::new(key).monospace().strong());
                ui.label(desc);
                ui.end_row();
            }
        });
}

/// Draw a tiny progress bar in the menu bar.
fn perf_bar(ui: &mut egui::Ui, fraction: f32, width: f32, color: egui::Color32) {
    let height = 12.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    let painter = ui.painter();

    // Background
    painter.rect_filled(rect, 2.0, egui::Color32::from_gray(40));

    // Filled portion
    let fill_frac = fraction.clamp(0.0, 1.0);
    if fill_frac > 0.0 {
        let fill_rect = egui::Rect::from_min_size(
            rect.min,
            egui::vec2(rect.width() * fill_frac, rect.height()),
        );
        painter.rect_filled(fill_rect, 2.0, color);
    }
}

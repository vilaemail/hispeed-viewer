use crate::platform::PerfMonitor;
use crate::settings::{CacheStorageMode, CompressionDevice, Settings, SourceType, VideoSettings};
use crate::source::traits::*;
use crate::source::{adb::AdbSource, folder::FolderSource, http::HttpSource};
use crate::sync::file_sync::{FileSync, SyncedFile};
use crate::ui::offset_window::{OffsetAction, SetOffsetState};
use crate::ui::setting_widgets::{OffsetInlineAction, OffsetInlineState};
use crate::settings::CompressionAlgorithm;
use crate::ui::settings_window::{BenchmarkMode, SettingsAction, SettingsValidationState};
use crate::ui::sync_status::PendingChange as UiPendingChange;
use crate::video::cache::{FrameCache, FrameTexture};
use crate::video::extractor::{ExtractionProgress, FrameExtractor};
use crate::video::gpu_device::{GpuAvailability, GpuCompressor};
use crate::video::metadata::VideoMetadata;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

// ── Benchmark types ──────────────────────────────────────────────────

struct BenchmarkRun {
    algo: CompressionAlgorithm,
    level: u32,
    device: CompressionDevice,
}

pub struct BenchmarkResult {
    pub algo: CompressionAlgorithm,
    pub level: u32,
    pub device: CompressionDevice,
    pub extraction_secs: f64,
    pub load_secs: f64,
    pub frames_loaded: u32,
    pub total_frames: u32,
    pub cache_bytes: u64,
    pub gpu_ram_bytes: u64,
    pub gpu_ram_per_frame: u64,
}

pub struct BenchmarkResults {
    pub video_name: String,
    pub total_frames: u32,
    pub frame_width: u32,
    pub frame_height: u32,
    pub storage_mode: CacheStorageMode,
    pub runs: Vec<BenchmarkResult>,
}

#[derive(PartialEq)]
enum BenchmarkPhase {
    Extracting,
    Loading,
    MeasuringVram,
}

struct BenchmarkState {
    queue: std::collections::VecDeque<BenchmarkRun>,
    results: Vec<BenchmarkResult>,
    storage_mode: CacheStorageMode,
    original_algo: CompressionAlgorithm,
    original_level: u32,
    original_device: CompressionDevice,
    recording_index: usize,
    // Current run tracking
    phase: BenchmarkPhase,
    current_algo: CompressionAlgorithm,
    current_level: u32,
    extraction_time: f64,
    load_start: Option<Instant>,
    last_loaded: u32,
    stall_since: Option<Instant>,
    vram_before_mb: Option<f32>,
    vram_settle_start: Option<Instant>,
    pending_load_secs: f64,
    pending_cache_bytes: u64,
    // Video info
    video_name: String,
    total_frames: u32,
    frame_width: u32,
    frame_height: u32,
    // Progress
    total_runs: usize,
    completed_runs: usize,
}

// ── App ──────────────────────────────────────────────────────────────

pub struct App {
    // Core state
    settings: Settings,
    settings_path: PathBuf,
    exe_dir: PathBuf,

    // GPU compression
    gpu_availability: GpuAvailability,
    gpu_compressor: Option<Arc<Mutex<GpuCompressor>>>,

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
    extraction_start: Option<Instant>,

    // Background compressed→raw decompression
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

    // File browser state
    sort_recordings_by_original: bool,
    rename_state: crate::ui::file_browser::RenameState,
    properties_state: crate::ui::file_browser::PropertiesState,

    // Pending reopen after settings change
    pending_reopen: bool,

    // Benchmark state
    benchmark: Option<BenchmarkState>,
    benchmark_pending_next: bool,
    benchmark_results: Option<BenchmarkResults>,

    // Exit cleanup dialog
    show_exit_dialog: bool,
    exit_confirmed: bool,
    exit_cleanup_sizes: Option<(u64, u64, u64)>, // cache, videos, temp
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

        // Probe GPU availability
        let gpu_availability = GpuAvailability::probe();

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
            gpu_availability,
            gpu_compressor: None,
            frame_cache: FrameCache::new(),
            current_frame: 0,
            total_frames: 0,
            current_metadata: None,
            current_recording_index: None,
            overlay_pos,
            extraction_rx: None,
            extraction_cancel: None,
            extraction_progress: None,
            extraction_start: None,
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
            sort_recordings_by_original: true,
            rename_state: crate::ui::file_browser::RenameState::default(),
            properties_state: crate::ui::file_browser::PropertiesState::default(),
            pending_reopen: false,
            benchmark: None,
            benchmark_pending_next: false,
            benchmark_results: None,
            show_exit_dialog: false,
            exit_confirmed: false,
            exit_cleanup_sizes: None,
        }
    }

    /// Return an Arc<Mutex<GpuCompressor>> if the current device setting is a GPU.
    /// Lazily creates the compressor on first call; returns None and falls back to CPU on failure.
    fn ensure_gpu_compressor(&mut self) -> Option<Arc<Mutex<GpuCompressor>>> {
        if self.settings.compression_device == CompressionDevice::Cpu {
            return None;
        }
        if !self.settings.compression_algorithm.is_gpu_compressed() {
            return None;
        }

        let desired_pref = match self.settings.compression_device {
            CompressionDevice::Gpu => wgpu::PowerPreference::HighPerformance,
            CompressionDevice::Igpu => wgpu::PowerPreference::LowPower,
            CompressionDevice::Cpu => return None,
        };

        // Reuse existing compressor if power preference matches
        if let Some(ref comp) = self.gpu_compressor {
            if comp.lock().unwrap().power_preference() == desired_pref {
                return Some(comp.clone());
            }
            // Different preference — drop and recreate
            self.gpu_compressor = None;
        }

        // Create new compressor
        match crate::video::gpu_device::create_gpu_compressor(self.settings.compression_device) {
            Some(comp) => {
                self.gpu_compressor = Some(comp.clone());
                Some(comp)
            }
            None => {
                let fallback = self.gpu_availability.best_fallback(self.settings.compression_device);
                if fallback != self.settings.compression_device {
                    log::warn!(
                        "GPU compressor creation failed for {:?}, falling back to {:?}",
                        self.settings.compression_device,
                        fallback,
                    );
                    self.settings.compression_device = fallback;
                    if fallback == CompressionDevice::Cpu {
                        return None;
                    }
                    // Try fallback device
                    match crate::video::gpu_device::create_gpu_compressor(fallback) {
                        Some(comp) => {
                            self.gpu_compressor = Some(comp.clone());
                            Some(comp)
                        }
                        None => {
                            log::error!("Fallback GPU compressor also failed, using CPU");
                            self.settings.compression_device = CompressionDevice::Cpu;
                            None
                        }
                    }
                } else {
                    log::error!("GPU compressor creation failed, using CPU");
                    self.settings.compression_device = CompressionDevice::Cpu;
                    None
                }
            }
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

        let algo = self.settings.compression_algorithm;
        let level = self.settings.compression_level;
        let storage_mode = self.settings.cache_storage_mode;

        // Check if frames are already extracted with matching settings
        if let Some(info) = FrameExtractor::is_already_extracted(&cache_dir, algo, level, storage_mode) {
            log::info!("Frames already extracted for {}", synced.json_name);
            crate::video::cache::touch_cache_access(&cache_dir);
            // Use actual dimensions from extraction (may differ from metadata)
            self.frame_cache.set_gpu_budget(self.settings.gpu_cache_mb, info.frame_width, info.frame_height, algo);
            self.frame_cache.set_source(&cache_dir, metadata.total_frames, info.frame_width, info.frame_height, algo, storage_mode);
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

            // Consistency cleanup based on storage mode
            let compressed_dir = cache_dir.join("compressed");
            let raw_dir = cache_dir.join("raw");
            match storage_mode {
                CacheStorageMode::CompressedOnly => {
                    if raw_dir.is_dir() {
                        log::info!("Removing stale raw cache (CompressedOnly mode)");
                        let _ = std::fs::remove_dir_all(&raw_dir);
                    }
                }
                CacheStorageMode::RawOnly => {
                    if compressed_dir.is_dir() {
                        log::info!("Removing stale compressed cache (RawOnly mode)");
                        let _ = std::fs::remove_dir_all(&compressed_dir);
                    }
                }
                CacheStorageMode::Both => {
                    // Start background decompression if raw cache is missing
                    if !FrameExtractor::has_raw_cache(&cache_dir) {
                        self.start_decompress(&cache_dir);
                    }
                }
            }

            // Clean up legacy lz4 dir
            let legacy_lz4_dir = cache_dir.join("lz4");
            if legacy_lz4_dir.is_dir() {
                let _ = std::fs::remove_dir_all(&legacy_lz4_dir);
            }
        } else {
            // No valid extraction or settings mismatch — clean up and re-extract
            log::info!("Cache invalid or missing for {}, starting extraction", synced.json_name);

            // Clean up all subdirs before fresh extraction
            let compressed_dir = cache_dir.join("compressed");
            let raw_dir = cache_dir.join("raw");
            let legacy_lz4_dir = cache_dir.join("lz4");
            if compressed_dir.is_dir() { let _ = std::fs::remove_dir_all(&compressed_dir); }
            if raw_dir.is_dir() { let _ = std::fs::remove_dir_all(&raw_dir); }
            if legacy_lz4_dir.is_dir() { let _ = std::fs::remove_dir_all(&legacy_lz4_dir); }
            let _ = std::fs::remove_file(cache_dir.join(".complete"));

            // Use metadata dimensions for GPU budget (will be updated after extraction)
            self.frame_cache.set_gpu_budget(self.settings.gpu_cache_mb, metadata.width, metadata.height, algo);

            // Proactively evict caches to make room for this video
            let estimated = crate::video::cache::estimate_cache_bytes(
                metadata.total_frames, metadata.width, metadata.height, storage_mode, algo,
            );
            let cache_root = self.settings.cache_dir(&self.exe_dir);
            crate::video::cache::evict_for_new_video(
                &cache_root,
                self.settings.disk_cache_mb,
                &metadata.sha256,
                estimated,
                storage_mode,
            );

            // Start extraction
            log::info!("Starting frame extraction for {}", synced.json_name);
            let gpu_comp = self.ensure_gpu_compressor();
            let extractor = FrameExtractor::new(&self.settings.ffmpeg_path);
            let (rx, cancel) = extractor.extract_async(
                synced.video_path.clone(),
                cache_dir.clone(),
                metadata.total_frames,
                metadata.width,
                metadata.height,
                algo,
                level,
                storage_mode,
                gpu_comp,
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
            self.extraction_start = Some(Instant::now());
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
        // Only decompress if Both mode (compressed → raw)
        if self.settings.cache_storage_mode != CacheStorageMode::Both {
            return;
        }
        // Cancel any existing decompression
        if let Some(cancel) = &self.decompress_cancel {
            cancel.store(true, Ordering::Relaxed);
        }
        if let Some(handle) = self.decompress_handle.take() {
            let _ = handle.join();
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let (fw, fh) = if let Some(meta) = &self.current_metadata {
            (meta.width, meta.height)
        } else {
            let dims = self.frame_cache.frame_dimensions();
            if dims.0 > 0 { dims } else { return; }
        };
        let handle = crate::video::cache::decompress_cache_async(
            cache_dir.to_path_buf(),
            cancel.clone(),
            self.settings.compression_algorithm,
            fw,
            fh,
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
        self.extraction_start = None;

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
                // Log extraction timing
                if let Some(start) = self.extraction_start {
                    let elapsed = start.elapsed().as_secs_f64();
                    log::info!("Extraction completed in {:.2}s ({} frames)", elapsed, progress.extracted_frames);
                    if let Some(bench) = &mut self.benchmark {
                        if bench.phase == BenchmarkPhase::Extracting {
                            bench.extraction_time = elapsed;
                        }
                    }
                }

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
                    if let Some(bench) = &mut self.benchmark {
                        bench.completed_runs += 1;
                        self.benchmark_pending_next = true;
                    }
                } else {
                    // Show warning if frame count mismatched (but still load)
                    if !progress.warning_message.is_empty() {
                        log::warn!("{}", progress.warning_message);
                        self.warning_banner = Some(progress.warning_message.clone());
                    }

                    let algo = self.settings.compression_algorithm;
                    let storage_mode = self.settings.cache_storage_mode;
                    let is_benchmarking = self.benchmark.is_some();

                    // Extraction done - set up frame cache
                    if let Some(meta) = &self.current_metadata {
                        let sha = meta.sha256.clone();
                        let total = meta.total_frames;
                        let (mw, mh) = (meta.width, meta.height);
                        let cache_dir = self.settings.cache_dir(&self.exe_dir).join(&sha);
                        crate::video::cache::touch_cache_access(&cache_dir);
                        // Use actual dimensions from extraction marker
                        let (fw, fh) = FrameExtractor::is_already_extracted(&cache_dir, algo, self.settings.compression_level, storage_mode)
                            .map(|info| (info.frame_width, info.frame_height))
                            .unwrap_or((mw, mh));
                        self.frame_cache.set_gpu_budget(self.settings.gpu_cache_mb, fw, fh, algo);
                        self.frame_cache.set_source(&cache_dir, total, fw, fh, algo, storage_mode);
                        self.frame_cache.set_current_frame(self.current_frame);

                        // Skip background decompression and disk cache management during benchmark
                        if !is_benchmarking {
                            self.start_decompress(&cache_dir);

                            let cache_root = self.settings.cache_dir(&self.exe_dir);
                            if let Err(e) = crate::video::cache::manage_disk_cache(
                                &cache_root,
                                self.settings.disk_cache_mb,
                                &sha,
                                storage_mode,
                            ) {
                                log::error!("{e}");
                                self.error_banner = Some(e);
                            }
                        }
                    }

                    // Transition benchmark to loading phase
                    if let Some(bench) = &mut self.benchmark {
                        if bench.phase == BenchmarkPhase::Extracting {
                            bench.phase = BenchmarkPhase::Loading;
                            bench.load_start = Some(Instant::now());
                            bench.last_loaded = 0;
                            bench.stall_since = None;
                            // Snapshot VRAM before loading frames
                            bench.vram_before_mb = crate::platform::windows::gpu_memory_info().map(|(used, _)| used);
                        }
                    }
                }
                self.extraction_rx = None;
                self.extraction_cancel = None;
                self.extraction_progress = None;
                self.extraction_start = None;
            } else {
                self.extraction_progress = Some(progress);
            }
        }
    }

    /// Begin the benchmark load-monitoring phase (called when extraction completes).
    /// The prefetch system is already running; we just start tracking its progress.
    /// Poll the prefetch system's progress during benchmark load phase.
    fn update_benchmark_load(&mut self) {
        let bench = match &mut self.benchmark {
            Some(b) if b.phase == BenchmarkPhase::Loading => b,
            _ => return,
        };

        let Some(load_start) = bench.load_start else { return };
        let loaded = self.frame_cache.loaded_frame_count();
        let total = self.total_frames;
        let all_loaded = loaded >= total;

        if loaded > bench.last_loaded {
            bench.last_loaded = loaded;
            bench.stall_since = None;
        } else if bench.stall_since.is_none() {
            bench.stall_since = Some(Instant::now());
        }

        let stalled = bench.stall_since
            .map(|s| s.elapsed().as_secs_f64() > 1.0)
            .unwrap_or(false);

        if all_loaded || stalled {
            let load_elapsed = load_start.elapsed().as_secs_f64();

            // Measure cache disk size
            let cache_dir = self.settings.cache_dir(&self.exe_dir);
            let sha = self.current_metadata.as_ref().map(|m| m.sha256.clone()).unwrap_or_default();
            let cache_bytes = dir_size_bytes(&cache_dir.join(&sha));

            let bench = self.benchmark.as_mut().unwrap();
            bench.pending_load_secs = load_elapsed;
            bench.pending_cache_bytes = cache_bytes;

            log::info!(
                "Benchmark run: {} level {} — load complete, waiting 4s for VRAM to stabilize...",
                bench.current_algo.label(), bench.current_level,
            );

            // Transition to VRAM measurement phase — wait for GPU memory to stabilize
            bench.phase = BenchmarkPhase::MeasuringVram;
            bench.vram_settle_start = Some(Instant::now());
        }
    }

    /// Wait for VRAM to stabilize after loading, then record the result.
    fn update_benchmark_vram(&mut self) {
        let bench = match &mut self.benchmark {
            Some(b) if b.phase == BenchmarkPhase::MeasuringVram => b,
            _ => return,
        };

        let Some(settle_start) = bench.vram_settle_start else { return };
        if settle_start.elapsed().as_secs_f64() < 4.0 {
            return; // Still waiting
        }

        let loaded = self.frame_cache.loaded_frame_count();
        let total = self.total_frames;
        let load_elapsed = bench.pending_load_secs;
        let cache_bytes = bench.pending_cache_bytes;
        let ext_time = bench.extraction_time;
        let ext_fps = if ext_time > 0.0 { total as f64 / ext_time } else { 0.0 };
        let load_fps = if load_elapsed > 0.0 { loaded as f64 / load_elapsed } else { 0.0 };

        log::info!(
            "Benchmark run: {} level {} — extraction {:.2}s ({:.1} fps), load {}/{} {:.2}s ({:.1} fps), cache {:.1} MB",
            bench.current_algo.label(), bench.current_level,
            ext_time, ext_fps, loaded, total, load_elapsed, load_fps,
            cache_bytes as f64 / (1024.0 * 1024.0),
        );

        // Measure VRAM experimentally via DXGI delta
        let gpu_ram = if let Some(before) = bench.vram_before_mb {
            let after = crate::platform::windows::gpu_memory_info()
                .map(|(used, _)| used)
                .unwrap_or(before);
            let delta_mb = (after - before).max(0.0);
            (delta_mb * 1024.0 * 1024.0) as u64
        } else {
            // Fallback to calculated if DXGI unavailable
            self.frame_cache.cached_bytes()
        };
        let gpu_per_frame = if loaded > 0 { gpu_ram / loaded as u64 } else { 0 };

        bench.results.push(BenchmarkResult {
            algo: bench.current_algo,
            level: bench.current_level,
            device: self.settings.compression_device,
            extraction_secs: ext_time,
            load_secs: load_elapsed,
            frames_loaded: loaded,
            total_frames: total,
            cache_bytes,
            gpu_ram_bytes: gpu_ram,
            gpu_ram_per_frame: gpu_per_frame,
        });
        bench.completed_runs += 1;

        self.benchmark_pending_next = true;
    }

    /// Start the benchmark suite.
    fn start_benchmark(&mut self, mode: BenchmarkMode, ctx: &egui::Context) {
        self.close_recording();

        if self.recordings.is_empty() {
            self.error_banner = Some("No recordings available for benchmark".into());
            return;
        }

        let current_device = self.settings.compression_device;
        let best_gpu = self.gpu_availability.best_fallback(CompressionDevice::Gpu);

        let mut queue = std::collections::VecDeque::new();
        match mode {
            BenchmarkMode::CurrentSettings => {
                queue.push_back(BenchmarkRun {
                    algo: self.settings.compression_algorithm,
                    level: self.settings.compression_level,
                    device: current_device,
                });
            }
            BenchmarkMode::SmallSubset => {
                // Non-BC runs use CPU
                queue.push_back(BenchmarkRun { algo: CompressionAlgorithm::Lz4, level: 0, device: CompressionDevice::Cpu });
                queue.push_back(BenchmarkRun { algo: CompressionAlgorithm::Png, level: 0, device: CompressionDevice::Cpu });
                queue.push_back(BenchmarkRun { algo: CompressionAlgorithm::Png, level: 3, device: CompressionDevice::Cpu });
                queue.push_back(BenchmarkRun { algo: CompressionAlgorithm::Zstd, level: 1, device: CompressionDevice::Cpu });
                queue.push_back(BenchmarkRun { algo: CompressionAlgorithm::Zstd, level: 3, device: CompressionDevice::Cpu });
                // BC runs use current device setting
                queue.push_back(BenchmarkRun { algo: CompressionAlgorithm::Bc1, level: 0, device: current_device });
                queue.push_back(BenchmarkRun { algo: CompressionAlgorithm::Bc7, level: 0, device: current_device });
                if current_device != CompressionDevice::Cpu {
                    // GPU is fast enough to also test Very Fast and Fast
                    queue.push_back(BenchmarkRun { algo: CompressionAlgorithm::Bc7, level: 1, device: current_device });
                    queue.push_back(BenchmarkRun { algo: CompressionAlgorithm::Bc7, level: 2, device: current_device });
                }
            }
            BenchmarkMode::All => {
                queue.push_back(BenchmarkRun { algo: CompressionAlgorithm::Lz4, level: 0, device: CompressionDevice::Cpu });
                for level in 1..=22 {
                    queue.push_back(BenchmarkRun { algo: CompressionAlgorithm::Zstd, level, device: CompressionDevice::Cpu });
                }
                for level in 0..=9 {
                    queue.push_back(BenchmarkRun { algo: CompressionAlgorithm::Png, level, device: CompressionDevice::Cpu });
                }
                // BC runs: test both CPU and best available GPU
                queue.push_back(BenchmarkRun { algo: CompressionAlgorithm::Bc1, level: 0, device: CompressionDevice::Cpu });
                for level in 0..=4 {
                    queue.push_back(BenchmarkRun { algo: CompressionAlgorithm::Bc7, level, device: CompressionDevice::Cpu });
                }
                if best_gpu != CompressionDevice::Cpu {
                    queue.push_back(BenchmarkRun { algo: CompressionAlgorithm::Bc1, level: 0, device: best_gpu });
                    for level in 0..=4 {
                        queue.push_back(BenchmarkRun { algo: CompressionAlgorithm::Bc7, level, device: best_gpu });
                    }
                }
            }
        }

        let total_runs = queue.len();
        let rec = &self.recordings[0];
        let video_name = rec.json_name.trim_end_matches(".json").to_string();

        let state = BenchmarkState {
            queue,
            results: Vec::new(),
            storage_mode: self.settings.cache_storage_mode,
            original_algo: self.settings.compression_algorithm,
            original_level: self.settings.compression_level,
            original_device: self.settings.compression_device,
            recording_index: 0,
            phase: BenchmarkPhase::Extracting,
            current_algo: CompressionAlgorithm::Lz4,
            current_level: 0,
            extraction_time: 0.0,
            load_start: None,
            last_loaded: 0,
            stall_since: None,
            vram_before_mb: None,
            vram_settle_start: None,
            pending_load_secs: 0.0,
            pending_cache_bytes: 0,
            video_name,
            total_frames: rec.metadata.total_frames,
            frame_width: rec.metadata.width,
            frame_height: rec.metadata.height,
            total_runs,
            completed_runs: 0,
        };

        self.benchmark = Some(state);
        self.start_next_benchmark_run(ctx);
    }

    /// Pop the next run from the queue and start it, or finish if queue is empty.
    fn start_next_benchmark_run(&mut self, ctx: &egui::Context) {
        let bench = self.benchmark.as_mut().unwrap();

        let next_run = bench.queue.pop_front();
        if let Some(run) = next_run {
            bench.current_algo = run.algo;
            bench.current_level = run.level;
            bench.phase = BenchmarkPhase::Extracting;
            bench.extraction_time = 0.0;
            bench.load_start = None;
            bench.last_loaded = 0;
            bench.stall_since = None;
            bench.vram_settle_start = None;
            bench.pending_load_secs = 0.0;
            bench.pending_cache_bytes = 0;
            let recording_index = bench.recording_index;

            // Drop the borrow on self.benchmark before calling self methods
            self.settings.compression_algorithm = run.algo;
            self.settings.compression_level = run.level;
            self.settings.compression_device = run.device;
            // Force CompressedOnly for BC formats (no raw path)
            if run.algo.is_gpu_compressed() {
                self.settings.cache_storage_mode = CacheStorageMode::CompressedOnly;
            }
            // Force re-creation of GPU compressor if device changed
            self.gpu_compressor = None;

            // Eagerly warm up GPU compressor so pipeline creation time
            // (BC7 DX12 shader compilation can take 20+ seconds) doesn't
            // pollute the benchmark extraction timer.
            if run.algo.is_gpu_compressed() && run.device != CompressionDevice::Cpu {
                let _ = self.ensure_gpu_compressor();
            }

            self.close_recording();

            let rec = self.recordings[recording_index].clone();
            let cache_dir = self.settings.cache_dir(&self.exe_dir).join(&rec.metadata.sha256);
            if cache_dir.is_dir() {
                let _ = std::fs::remove_dir_all(&cache_dir);
            }

            self.current_recording_index = Some(recording_index);
            self.open_recording(&rec, ctx);
        } else {
            self.finish_all_benchmarks();
        }
    }

    /// All runs complete — restore settings and show results.
    fn finish_all_benchmarks(&mut self) {
        let bench = self.benchmark.take().unwrap();

        // Restore original compression and storage settings
        self.settings.compression_algorithm = bench.original_algo;
        self.settings.compression_level = bench.original_level;
        self.settings.cache_storage_mode = bench.storage_mode;
        self.settings.compression_device = bench.original_device;
        self.gpu_compressor = None; // Force re-creation with restored device

        self.close_recording();

        self.benchmark_results = Some(BenchmarkResults {
            video_name: bench.video_name,
            total_frames: bench.total_frames,
            frame_width: bench.frame_width,
            frame_height: bench.frame_height,
            storage_mode: bench.storage_mode,
            runs: bench.results,
        });
    }

    /// Delete all `compressed/` subdirs and `.complete` markers in cache
    /// (called when compression algorithm or level changes).
    fn cleanup_compressed_cache(&self) {
        let cache_root = self.settings.cache_dir(&self.exe_dir);
        if let Ok(entries) = std::fs::read_dir(&cache_root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let compressed = path.join("compressed");
                    if compressed.is_dir() {
                        let _ = std::fs::remove_dir_all(&compressed);
                    }
                    let _ = std::fs::remove_file(path.join(".complete"));
                }
            }
        }
    }

    /// Delete dirs that conflict with the current storage mode across all video caches.
    fn cleanup_storage_mode_cache(&self) {
        let cache_root = self.settings.cache_dir(&self.exe_dir);
        if let Ok(entries) = std::fs::read_dir(&cache_root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    match self.settings.cache_storage_mode {
                        CacheStorageMode::CompressedOnly => {
                            let raw = path.join("raw");
                            if raw.is_dir() { let _ = std::fs::remove_dir_all(&raw); }
                        }
                        CacheStorageMode::RawOnly => {
                            let comp = path.join("compressed");
                            if comp.is_dir() { let _ = std::fs::remove_dir_all(&comp); }
                        }
                        CacheStorageMode::Both => {}
                    }
                }
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

    /// Build the current per-video settings struct, preserving display_name.
    fn current_video_settings(&self) -> VideoSettings {
        let existing = self.current_video_path.as_ref()
            .map(|p| VideoSettings::load(p))
            .unwrap_or_default();
        VideoSettings {
            time_offset_seconds: self.time_offset_seconds,
            overlay_x: self.overlay_pos.x,
            overlay_y: self.overlay_pos.y,
            display_name: existing.display_name,
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
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        // Intercept window close for exit cleanup dialog
        if ctx.input(|i| i.viewport().close_requested()) && !self.exit_confirmed {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);

            let cache_dir = self.settings.cache_dir(&self.exe_dir);
            let completed_dir = self.settings.completed_dir(&self.exe_dir);
            let temp_dir = self.settings.temp_dir(&self.exe_dir);

            let cache_size = crate::video::cache::get_dir_size(&cache_dir);
            let completed_size = crate::video::cache::get_dir_size(&completed_dir);
            let temp_size = crate::video::cache::get_dir_size(&temp_dir);
            let total = cache_size + completed_size + temp_size;

            let total_gb = total as f64 / (1024.0 * 1024.0 * 1024.0);
            if total_gb < 0.01 {
                // Nothing significant, just exit
                self.exit_confirmed = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            } else {
                self.exit_cleanup_sizes = Some((cache_size, completed_size, temp_size));
                self.show_exit_dialog = true;
            }
        }

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
        let has_pending = self.frame_cache.process_uploads(ctx, frame, 4.0);

        // Update benchmark load progress (after uploads so loaded count is current)
        self.update_benchmark_load();
        self.update_benchmark_vram();

        // Start next benchmark run if flagged (needs ctx for open_recording)
        if self.benchmark_pending_next {
            self.benchmark_pending_next = false;
            self.start_next_benchmark_run(ctx);
        }

        // Request repaint if we have active work
        if has_pending || self.extraction_progress.is_some() || self.extraction_rx.is_some() || self.is_playing || self.benchmark.is_some() {
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
                    // Fixed-width GB formatter: "XXX.XX" (6 chars) or placeholder
                    let fmt_gb = |val: Option<f64>| -> String {
                        match val {
                            Some(v) => format!("{:6.2}", v),
                            None => "  ?.??".to_string(),
                        }
                    };

                    let blue = egui::Color32::from_rgb(60, 100, 200);
                    let green = egui::Color32::from_rgb(60, 160, 60);
                    let yellow = egui::Color32::from_rgb(200, 180, 40);
                    let red = egui::Color32::from_rgb(200, 60, 60);

                    // Cache disk (always shown)
                    {
                        let (used, limit, frac) = if let Some((used_mb, limit_mb)) = perf.cache_disk {
                            let f = if limit_mb > 0.0 { used_mb / limit_mb } else { 0.0 };
                            (Some(used_mb as f64 / 1024.0), Some(limit_mb as f64 / 1024.0), f)
                        } else {
                            (None, None, 0.0)
                        };
                        ui.label(mono(format!("Cache: {}/{} GB", fmt_gb(used), fmt_gb(limit))));
                        perf_bar(ui, frac, 40.0, blue);
                    }

                    ui.label(sep());

                    // Video disk (always shown)
                    {
                        let (folder, free, frac) = if let Some((folder_bytes, free_bytes)) = perf.video_disk {
                            let fg = folder_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
                            let frg = free_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
                            let f = if free_bytes > 0 { folder_bytes as f32 / free_bytes as f32 } else { 0.0 };
                            (Some(fg), Some(frg), f)
                        } else {
                            (None, None, 0.0)
                        };
                        ui.label(mono(format!("DISK: {} / {} GB", fmt_gb(folder), fmt_gb(free))));
                        perf_bar(ui, frac, 40.0, blue);
                    }

                    ui.label(sep());

                    // VRAM (always shown)
                    {
                        let (used, total, frac) = if let (Some(u), Some(t)) = (perf.gpu_memory_mb, perf.gpu_total_mb) {
                            let f = if t > 0.0 { u / t } else { 0.0 };
                            (Some(u as f64 / 1024.0), Some(t as f64 / 1024.0), f)
                        } else {
                            (None, None, 0.0)
                        };
                        ui.label(mono(format!("VRAM: {}/{} GB", fmt_gb(used), fmt_gb(total))));
                        perf_bar(ui, frac, 40.0, green);
                    }

                    ui.label(sep());

                    // Memory (always shown)
                    {
                        let frac = if perf.total_memory_mb > 0.0 {
                            perf.memory_mb / perf.total_memory_mb
                        } else {
                            0.0
                        };
                        let mem_gb = perf.memory_mb as f64 / 1024.0;
                        let total_gb = perf.total_memory_mb as f64 / 1024.0;
                        ui.label(mono(format!("MEM: {:6.2}/{:6.2} GB", mem_gb, total_gb)));
                        perf_bar(ui, frac, 40.0, yellow);
                    }

                    ui.label(sep());

                    // GPU utilization (always shown)
                    {
                        let gpu_pct = match perf.gpu_percent {
                            Some(p) => format!("{:3.0}", p),
                            None => "  ?".to_string(),
                        };
                        let gpu_frac = perf.gpu_percent.unwrap_or(0.0) / 100.0;
                        ui.label(mono(format!("GPU: {}%", gpu_pct)));
                        perf_bar(ui, gpu_frac, 40.0, red);
                    }

                    ui.label(sep());

                    // CPU (always shown)
                    {
                        let cpu_frac = perf.cpu_percent / 100.0;
                        ui.label(mono(format!("CPU: {:3.0}%", perf.cpu_percent)));
                        perf_bar(ui, cpu_frac, 40.0, red);
                    }

                    ui.label(sep());

                    ui.label(mono(format!("FPS: {:3.0}", perf.fps)));
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
                    cache.cached_count(),
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
                // Reserve bottom section for folder shortcuts
                egui::TopBottomPanel::bottom("folder_links")
                    .show_inside(ui, |ui| {
                        ui.add_space(2.0);
                        ui.horizontal(|ui| {
                            let videos_dir = self.settings.completed_dir(&self.exe_dir);
                            if ui.link("Videos").on_hover_text(videos_dir.display().to_string()).clicked() {
                                crate::platform::open_folder(&videos_dir);
                            }
                            ui.separator();
                            let cache_dir = self.settings.cache_dir(&self.exe_dir);
                            if ui.link("Cache").on_hover_text(cache_dir.display().to_string()).clicked() {
                                crate::platform::open_folder(&cache_dir);
                            }
                        });
                        ui.add_space(2.0);
                    });

                // File browser in remaining space
                let cache_dir = self.settings.cache_dir(&self.exe_dir);
                let algo = self.settings.compression_algorithm;
                let level = self.settings.compression_level;
                let storage_mode = self.settings.cache_storage_mode;

                let mut browser_recordings: Vec<crate::ui::file_browser::CompletedRecording> = self
                    .recordings
                    .iter()
                    .map(|r| {
                        let original_name = r.json_name.trim_end_matches(".json").to_string();
                        let vs = VideoSettings::load(&r.video_path);
                        let has_custom_name = vs.display_name.is_some();
                        let display_name = vs.display_name.unwrap_or_else(|| original_name.clone());
                        let vid_cache_dir = cache_dir.join(&r.metadata.sha256);
                        let is_cached = FrameExtractor::is_already_extracted(&vid_cache_dir, algo, level, storage_mode).is_some();
                        crate::ui::file_browser::CompletedRecording {
                            json_path: r.json_path.clone(),
                            video_path: r.video_path.clone(),
                            original_name,
                            display_name,
                            has_custom_name,
                            file_size: r.metadata.size,
                            metadata: r.metadata.clone(),
                            is_cached,
                        }
                    })
                    .collect();

                if self.sort_recordings_by_original {
                    browser_recordings.sort_by(|a, b| b.original_name.cmp(&a.original_name));
                } else {
                    browser_recordings.sort_by(|a, b| b.display_name.cmp(&a.display_name));
                }

                let browser_action = crate::ui::file_browser::render_file_browser(
                    ui,
                    &browser_recordings,
                    self.current_recording_index,
                    &mut self.sort_recordings_by_original,
                    &mut self.rename_state,
                    &mut self.properties_state,
                );

                match browser_action {
                    crate::ui::file_browser::FileBrowserAction::Open(idx) => {
                        if idx < self.recordings.len() {
                            let rec = self.recordings[idx].clone();
                            self.current_recording_index = Some(idx);
                            self.open_recording(&rec, ctx);
                        }
                    }
                    crate::ui::file_browser::FileBrowserAction::Rename { index, new_name } => {
                        if index < self.recordings.len() {
                            let video_path = &self.recordings[index].video_path;
                            let mut vs = VideoSettings::load(video_path);
                            vs.display_name = Some(new_name);
                            if let Err(e) = vs.save(video_path) {
                                log::error!("Failed to save rename: {e}");
                            }
                        }
                    }
                    crate::ui::file_browser::FileBrowserAction::RestoreOriginalName(index) => {
                        if index < self.recordings.len() {
                            let video_path = &self.recordings[index].video_path;
                            let mut vs = VideoSettings::load(video_path);
                            vs.display_name = None;
                            if let Err(e) = vs.save(video_path) {
                                log::error!("Failed to save name restore: {e}");
                            }
                        }
                    }
                    crate::ui::file_browser::FileBrowserAction::None => {}
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
            let neighbor_textures: Vec<(u32, Option<FrameTexture>, bool)> =
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
        let mut settings_action = SettingsAction::None;
        if self.show_settings {
            egui::Window::new("Settings")
                .open(&mut self.show_settings)
                .resizable(true)
                .default_width(400.0)
                .show(ctx, |ui| {
                    settings_action = crate::ui::settings_window::render_settings(
                        ui,
                        &mut self.settings_draft,
                        &self.settings,
                        &self.exe_dir,
                        &mut self.settings_validation,
                        &self.gpu_availability,
                    );
                });
        }

        match settings_action {
            SettingsAction::Save => {
                // Remember old settings for change detection
                let old_algo = self.settings.compression_algorithm;
                let old_level = self.settings.compression_level;
                let old_mode = self.settings.cache_storage_mode;
                let old_device = self.settings.compression_device;

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
                        self.settings.compression_algorithm,
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

                // Check if compression/storage settings changed
                let algo_changed = self.settings.compression_algorithm != old_algo
                    || self.settings.compression_level != old_level;
                let mode_changed = self.settings.cache_storage_mode != old_mode;
                let device_changed = self.settings.compression_device != old_device;

                if algo_changed {
                    self.cleanup_compressed_cache();
                }
                if mode_changed {
                    self.cleanup_storage_mode_cache();
                }
                // Invalidate GPU compressor if device changed (force re-creation)
                if device_changed {
                    self.gpu_compressor = None;
                }
                if algo_changed || mode_changed || device_changed {
                    self.pending_reopen = true;
                }
            }
            SettingsAction::Benchmark(mode) => {
                self.start_benchmark(mode, ctx);
            }
            SettingsAction::None => {}
        }

        // Handle pending reopen after settings change
        if self.pending_reopen {
            self.pending_reopen = false;
            if let Some(idx) = self.current_recording_index {
                if idx < self.recordings.len() {
                    let rec = self.recordings[idx].clone();
                    self.close_recording();
                    self.current_recording_index = Some(idx);
                    self.open_recording(&rec, ctx);
                }
            }
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

        // Benchmark progress overlay
        if let Some(bench) = &self.benchmark {
            let run_num = bench.completed_runs + 1;
            let total_runs = bench.total_runs;
            let algo_label = bench.current_algo.label();
            let level = bench.current_level;

            egui::Window::new("Benchmark")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(format!(
                        "Run {run_num}/{total_runs}: {algo_label} Level {level}"
                    ));
                    ui.add_space(4.0);
                    match bench.phase {
                        BenchmarkPhase::Extracting => {
                            if let Some(prog) = &self.extraction_progress {
                                let total = prog.total_frames.max(1);
                                ui.label(format!(
                                    "Extracting frames: {}/{}",
                                    prog.extracted_frames, total
                                ));
                                ui.add(egui::ProgressBar::new(
                                    prog.extracted_frames as f32 / total as f32,
                                ));
                            } else {
                                ui.label("Extracting frames...");
                                ui.add(egui::ProgressBar::new(0.0));
                            }
                        }
                        BenchmarkPhase::Loading => {
                            let loaded = self.frame_cache.loaded_frame_count();
                            let total = self.total_frames.max(1);
                            ui.label(format!("Loading frames to GPU: {loaded}/{total}"));
                            ui.add(egui::ProgressBar::new(loaded as f32 / total as f32));
                        }
                        BenchmarkPhase::MeasuringVram => {
                            let elapsed = bench.vram_settle_start
                                .map(|s| s.elapsed().as_secs_f64())
                                .unwrap_or(0.0);
                            let progress = (elapsed / 4.0).min(1.0) as f32;
                            ui.label(format!("Measuring VRAM ({:.1}s / 4.0s)...", elapsed));
                            ui.add(egui::ProgressBar::new(progress));
                        }
                    }
                    // Overall progress
                    ui.add_space(4.0);
                    let overall = bench.completed_runs as f32 / bench.total_runs.max(1) as f32;
                    ui.label(format!(
                        "Overall: {}/{} runs complete",
                        bench.completed_runs, bench.total_runs
                    ));
                    ui.add(egui::ProgressBar::new(overall));
                });
            ctx.request_repaint();
        }

        // Benchmark results table
        if self.benchmark_results.is_some() {
            let mut close_results = false;
            let mut open = true;
            egui::Window::new("Benchmark Results")
                .open(&mut open)
                .collapsible(false)
                .resizable(true)
                .default_width(700.0)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    let results = self.benchmark_results.as_ref().unwrap();
                    ui.label(format!(
                        "Video: {}  |  {}x{}  |  {} frames  |  Mode: {}",
                        results.video_name,
                        results.frame_width,
                        results.frame_height,
                        results.total_frames,
                        results.storage_mode.label(),
                    ));
                    ui.add_space(4.0);

                    egui::ScrollArea::vertical().max_height(400.0).show(ui, |ui| {
                        let raw_frame_bytes = results.frame_width as u64 * results.frame_height as u64 * 4;
                        egui::Grid::new("benchmark_results_grid")
                            .num_columns(12)
                            .spacing([12.0, 4.0])
                            .striped(true)
                            .show(ui, |ui| {
                                // Header
                                ui.strong("Algorithm");
                                ui.strong("Level");
                                ui.strong("Device");
                                ui.strong("Extract (s)");
                                ui.strong("Extract fps");
                                ui.strong("Load (s)");
                                ui.strong("Load fps");
                                ui.strong("Loaded");
                                ui.strong("Cache (MB)");
                                ui.strong("GPU RAM (MB)");
                                ui.strong("GPU/frame (KB)");
                                ui.strong("GPU vs RGBA %");
                                ui.end_row();

                                let results = self.benchmark_results.as_ref().unwrap();
                                for r in &results.runs {
                                    ui.label(r.algo.label());
                                    ui.label(r.algo.level_label(r.level));
                                    ui.label(r.device.label());
                                    ui.label(format!("{:.2}", r.extraction_secs));
                                    let ext_fps = if r.extraction_secs > 0.0 {
                                        r.total_frames as f64 / r.extraction_secs
                                    } else { 0.0 };
                                    ui.label(format!("{:.1}", ext_fps));
                                    ui.label(format!("{:.2}", r.load_secs));
                                    let load_fps = if r.load_secs > 0.0 {
                                        r.frames_loaded as f64 / r.load_secs
                                    } else { 0.0 };
                                    ui.label(format!("{:.1}", load_fps));
                                    ui.label(format!("{}/{}", r.frames_loaded, r.total_frames));
                                    ui.label(format!("{:.1}", r.cache_bytes as f64 / (1024.0 * 1024.0)));
                                    ui.label(format!("{:.1}", r.gpu_ram_bytes as f64 / (1024.0 * 1024.0)));
                                    ui.label(format!("{:.1}", r.gpu_ram_per_frame as f64 / 1024.0));
                                    let pct = if raw_frame_bytes > 0 {
                                        r.gpu_ram_per_frame as f64 / raw_frame_bytes as f64 * 100.0
                                    } else { 100.0 };
                                    ui.label(format!("{:.0}%", pct));
                                    ui.end_row();
                                }
                            });
                    });

                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Save as Markdown").clicked() {
                            let results = self.benchmark_results.as_ref().unwrap();
                            let md = generate_benchmark_markdown(results);
                            if let Some(path) = rfd::FileDialog::new()
                                .set_file_name("benchmark_results.md")
                                .add_filter("Markdown", &["md"])
                                .save_file()
                            {
                                if let Err(e) = std::fs::write(&path, md) {
                                    log::error!("Failed to save markdown: {e}");
                                } else {
                                    log::info!("Benchmark results saved to {}", path.display());
                                }
                            }
                        }
                        if ui.button("Close").clicked() {
                            close_results = true;
                        }
                    });
                });
            if !open || close_results {
                self.benchmark_results = None;
            }
        }

        // Exit cleanup dialog
        if self.show_exit_dialog {
            egui::Window::new("Exit Cleanup")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    if let Some((cache_size, completed_size, temp_size)) = self.exit_cleanup_sizes {
                        let total = cache_size + completed_size + temp_size;
                        let total_gb = total as f64 / (1024.0 * 1024.0 * 1024.0);
                        let cache_gb = cache_size as f64 / (1024.0 * 1024.0 * 1024.0);
                        let after_cache = (completed_size + temp_size) as f64 / (1024.0 * 1024.0 * 1024.0);

                        ui.label(format!(
                            "This software is heavy on HDD. {:.2} GB is currently on disk.",
                            total_gb
                        ));
                        ui.add_space(4.0);
                        ui.label(format!("  Cache: {:.2} GB", cache_gb));
                        ui.label(format!("  Videos + Temp: {:.2} GB", after_cache));
                        ui.add_space(8.0);

                        ui.vertical_centered(|ui| {
                            if ui.button(format!("Keep all ({:.2} GB left)", total_gb)).clicked() {
                                self.show_exit_dialog = false;
                                self.exit_confirmed = true;
                                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            }
                            ui.add_space(4.0);
                            if ui.button(format!("Remove cache ({:.2} GB left)", after_cache)).clicked() {
                                let cache_dir = self.settings.cache_dir(&self.exe_dir);
                                delete_dir_contents(&cache_dir);
                                self.show_exit_dialog = false;
                                self.exit_confirmed = true;
                                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            }
                            ui.add_space(4.0);
                            if ui.button("Remove all (nothing remains on disk)").clicked() {
                                let cache_dir = self.settings.cache_dir(&self.exe_dir);
                                let completed_dir = self.settings.completed_dir(&self.exe_dir);
                                let temp_dir = self.settings.temp_dir(&self.exe_dir);
                                delete_dir_contents(&cache_dir);
                                delete_dir_contents(&completed_dir);
                                delete_dir_contents(&temp_dir);
                                self.show_exit_dialog = false;
                                self.exit_confirmed = true;
                                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            }
                        });
                    }
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

/// Delete all contents of a directory but keep the directory itself.
/// Never deletes settings.json.
fn delete_dir_contents(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            if name == "settings.json" {
                continue;
            }
            if path.is_dir() {
                let _ = std::fs::remove_dir_all(&path);
            } else {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

/// Generate a Markdown table from benchmark results.
fn generate_benchmark_markdown(results: &BenchmarkResults) -> String {
    let mut md = String::new();
    md.push_str("# Benchmark Results\n\n");
    md.push_str(&format!(
        "**Video:** {}  \n**Resolution:** {}x{}  \n**Frames:** {}  \n**Storage Mode:** {}\n\n",
        results.video_name,
        results.frame_width,
        results.frame_height,
        results.total_frames,
        results.storage_mode.label(),
    ));
    let raw_frame_bytes = results.frame_width as u64 * results.frame_height as u64 * 4;
    md.push_str("| Algorithm | Level | Device | Extract (s) | Extract fps | Load (s) | Load fps | Loaded | Cache (MB) | GPU RAM (MB) | GPU/frame (KB) | GPU vs RGBA % |\n");
    md.push_str("|-----------|-------|--------|-------------|-------------|----------|----------|--------|------------|--------------|----------------|---------------|\n");
    for r in &results.runs {
        let ext_fps = if r.extraction_secs > 0.0 {
            r.total_frames as f64 / r.extraction_secs
        } else {
            0.0
        };
        let load_fps = if r.load_secs > 0.0 {
            r.frames_loaded as f64 / r.load_secs
        } else {
            0.0
        };
        let pct = if raw_frame_bytes > 0 {
            r.gpu_ram_per_frame as f64 / raw_frame_bytes as f64 * 100.0
        } else {
            100.0
        };
        md.push_str(&format!(
            "| {} | {} | {} | {:.2} | {:.1} | {:.2} | {:.1} | {}/{} | {:.1} | {:.1} | {:.1} | {:.0}% |\n",
            r.algo.label(),
            r.algo.level_label(r.level),
            r.device.label(),
            r.extraction_secs,
            ext_fps,
            r.load_secs,
            load_fps,
            r.frames_loaded,
            r.total_frames,
            r.cache_bytes as f64 / (1024.0 * 1024.0),
            r.gpu_ram_bytes as f64 / (1024.0 * 1024.0),
            r.gpu_ram_per_frame as f64 / 1024.0,
            pct,
        ));
    }
    md
}

/// Recursively compute total size in bytes of a directory.
fn dir_size_bytes(dir: &Path) -> u64 {
    let mut total = 0u64;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                total += dir_size_bytes(&path);
            } else if let Ok(meta) = entry.metadata() {
                total += meta.len();
            }
        }
    }
    total
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

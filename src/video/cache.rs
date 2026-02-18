use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use crate::settings::{CacheStorageMode, CompressionAlgorithm};

/// Number of frames around the cursor that are considered essential.
/// Essential frames can trigger LRU eviction of non-essential frames.
const ESSENTIAL_RADIUS: i32 = 15;

/// GL format constants for BC compression.
const GL_COMPRESSED_RGBA_S3TC_DXT1_EXT: i32 = 0x83F1;
const GL_COMPRESSED_RGBA_BPTC_UNORM: i32 = 0x8E8C;

/// A frame texture ready for rendering. Either managed by egui or a native GL texture.
#[derive(Clone)]
pub enum FrameTexture {
    Managed(egui::TextureHandle),
    Native {
        id: egui::TextureId,
        width: u32,
        height: u32,
    },
}

impl FrameTexture {
    pub fn id(&self) -> egui::TextureId {
        match self {
            Self::Managed(h) => h.id(),
            Self::Native { id, .. } => *id,
        }
    }

    pub fn size_vec2(&self) -> egui::Vec2 {
        match self {
            Self::Managed(h) => h.size_vec2(),
            Self::Native { width, height, .. } => egui::vec2(*width as f32, *height as f32),
        }
    }
}

/// A decoded frame ready to be turned into a GPU texture on the main thread.
pub struct DecodedFrame {
    pub frame_index: u32,
    pub color_image: Option<egui::ColorImage>,
    /// Raw BC compressed data (header-stripped, ready for GL upload).
    pub bc_data: Option<Vec<u8>>,
    /// Block-aligned width for BC textures (multiple of 4).
    pub bc_padded_width: u32,
    /// Block-aligned height for BC textures (multiple of 4).
    pub bc_padded_height: u32,
    pub width: u32,
    pub height: u32,
    /// True if the frame file was not found on disk.
    pub missing: bool,
}

/// Manages GPU textures for video frames with budget-based caching.
///
/// Essential frames (within ±15 of the cursor) are always cached and can trigger
/// LRU eviction of non-essential frames. Forward frames (cursor+16 to end of
/// video) are cached opportunistically when GPU budget allows, without evicting
/// anything. Background threads decode frame files in parallel for faster loading.
pub struct FrameCache {
    frame_dir: PathBuf,
    total_frames: u32,
    frame_width: u32,
    frame_height: u32,

    // GPU budget tracking (bytes)
    gpu_budget_bytes: u64,
    frame_bytes: u64,
    cached_bytes: u64,

    // GPU texture cache: frame_index -> CacheEntry
    textures: HashMap<u32, CacheEntry>,
    access_counter: u64,

    // Frames whose files were not found on disk
    missing_frames: HashSet<u32>,

    // Shared with prefetch thread: which frames are in GPU cache.
    // Prefetch reads this to skip already-cached frames (avoids wasted decodes).
    gpu_cached: Arc<RwLock<HashSet<u32>>>,

    // Communication with prefetch thread
    decoded_rx: Option<mpsc::Receiver<DecodedFrame>>,
    current_frame: Arc<AtomicI32>,
    running: Arc<AtomicBool>,
    cache_full: Arc<AtomicBool>,
    prefetch_handle: Option<std::thread::JoinHandle<()>>,

    // Compression settings for frame decoding
    compression_algorithm: CompressionAlgorithm,
    cache_storage_mode: CacheStorageMode,

    // Stored egui context for native texture cleanup during eviction/clear
    egui_ctx: Option<egui::Context>,

    // Stored glow context for direct GL texture deletion.
    // Native textures registered via `register_native_glow_texture` are NOT tracked
    // by egui's TextureManager, so `tex_manager.free()` is a no-op for them.
    // We must call `gl.delete_texture()` directly.
    gl_context: Option<std::sync::Arc<glow::Context>>,
}

struct CacheEntry {
    texture_handle: Option<egui::TextureHandle>,
    native_texture_id: Option<egui::TextureId>,
    /// Raw GL texture handle for direct deletion. Only set for native (BC) textures.
    gl_texture: Option<glow::Texture>,
    last_access: u64,
    bytes: u64,
}

impl FrameCache {
    pub fn new() -> Self {
        Self {
            frame_dir: PathBuf::new(),
            total_frames: 0,
            frame_width: 0,
            frame_height: 0,
            gpu_budget_bytes: 2048 * 1024 * 1024, // 2 GB default
            frame_bytes: 0,
            cached_bytes: 0,
            textures: HashMap::new(),
            access_counter: 0,
            missing_frames: HashSet::new(),
            gpu_cached: Arc::new(RwLock::new(HashSet::new())),
            decoded_rx: None,
            current_frame: Arc::new(AtomicI32::new(0)),
            running: Arc::new(AtomicBool::new(false)),
            cache_full: Arc::new(AtomicBool::new(false)),
            prefetch_handle: None,
            compression_algorithm: CompressionAlgorithm::default(),
            cache_storage_mode: CacheStorageMode::default(),
            egui_ctx: None,
            gl_context: None,
        }
    }

    /// Set GPU memory budget and frame dimensions for budget calculations.
    pub fn set_gpu_budget(&mut self, gpu_cache_mb: u32, frame_width: u32, frame_height: u32, algo: CompressionAlgorithm) {
        self.gpu_budget_bytes = gpu_cache_mb as u64 * 1024 * 1024;
        self.frame_bytes = algo.gpu_frame_bytes(frame_width, frame_height);
    }

    /// Set the source directory, total frame count, frame dimensions, and compression settings.
    /// Clears existing cache and restarts prefetch.
    pub fn set_source(
        &mut self,
        frame_dir: &Path,
        total_frames: u32,
        frame_width: u32,
        frame_height: u32,
        algo: CompressionAlgorithm,
        storage_mode: CacheStorageMode,
    ) {
        self.shutdown_prefetch();
        self.free_all_native_textures();
        self.textures.clear();
        self.missing_frames.clear();
        self.gpu_cached.write().unwrap().clear();
        self.access_counter = 0;
        self.cached_bytes = 0;
        self.cache_full.store(false, Ordering::Relaxed);
        self.frame_dir = frame_dir.to_path_buf();
        self.total_frames = total_frames;
        self.frame_width = frame_width;
        self.frame_height = frame_height;
        self.compression_algorithm = algo;
        self.cache_storage_mode = storage_mode;

        if total_frames > 0 && frame_dir.is_dir() {
            self.start_prefetch();
        }
    }

    /// Number of frames currently loaded in GPU cache.
    pub fn loaded_frame_count(&self) -> u32 {
        self.textures.len() as u32
    }

    /// Whether the GPU cache is full (no more frames can be loaded).
    pub fn is_cache_full(&self) -> bool {
        self.cache_full.load(Ordering::Relaxed)
    }

    /// Total bytes currently cached on the GPU.
    pub fn cached_bytes(&self) -> u64 {
        self.cached_bytes
    }

    /// Get the texture for a specific frame. Returns None if not yet loaded.
    pub fn get_frame(&mut self, frame_index: u32) -> Option<FrameTexture> {
        if let Some(entry) = self.textures.get_mut(&frame_index) {
            self.access_counter += 1;
            entry.last_access = self.access_counter;
            if let Some(handle) = &entry.texture_handle {
                return Some(FrameTexture::Managed(handle.clone()));
            }
            if let Some(id) = entry.native_texture_id {
                return Some(FrameTexture::Native {
                    id,
                    width: self.frame_width,
                    height: self.frame_height,
                });
            }
        }
        None
    }

    /// Notify the cache of the current frame position (triggers prefetch).
    pub fn set_current_frame(&self, frame_index: u32) {
        self.current_frame.store(frame_index as i32, Ordering::Relaxed);
    }

    /// Process pending decoded frames into GPU textures.
    /// Call this each frame from the main thread.
    ///
    /// Uses a time budget (in milliseconds) to limit how long uploads block the UI
    /// thread. Essential frames (±15 from cursor) can evict LRU frames outside the
    /// essential zone. Non-essential frames are only cached when there is room.
    ///
    /// Returns true if any frames were uploaded or more are pending.
    pub fn process_uploads(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame, budget_ms: f64) -> bool {
        // Store context for later use in eviction/cleanup
        if self.egui_ctx.is_none() {
            self.egui_ctx = Some(ctx.clone());
        }
        if self.gl_context.is_none() {
            self.gl_context = frame.gl().cloned();
        }

        // Temporarily take the receiver out of self so we can do mutable
        // operations (eviction, texture insert) while reading from the channel.
        let rx = match self.decoded_rx.take() {
            Some(rx) => rx,
            None => return false,
        };

        let start = Instant::now();
        let center = self.current_frame.load(Ordering::Relaxed);

        // If over budget (e.g. budget was lowered in settings), evict immediately
        if self.cached_bytes > self.gpu_budget_bytes {
            self.evict_n_outside(center, 10);
        }

        // Mark all frames in the essential zone (±15) as recently accessed.
        // This ensures the whole window around the cursor has high LRU priority,
        // not just the single frame being displayed.
        {
            self.access_counter += 1;
            let ac = self.access_counter;
            let ess_start = (center - ESSENTIAL_RADIUS).max(0) as u32;
            let ess_end = ((center + ESSENTIAL_RADIUS) as u32)
                .min(self.total_frames.saturating_sub(1));
            for idx in ess_start..=ess_end {
                if let Some(entry) = self.textures.get_mut(&idx) {
                    entry.last_access = ac;
                }
            }
        }

        let mut uploaded_any = false;
        let mut disconnected = false;
        let mut hit_budget = false;

        loop {
            if start.elapsed().as_secs_f64() * 1000.0 > budget_ms {
                hit_budget = true;
                break;
            }

            match rx.try_recv() {
                Ok(decoded) => {
                    if decoded.missing {
                        self.missing_frames.insert(decoded.frame_index);
                        continue;
                    }
                    if self.textures.contains_key(&decoded.frame_index) {
                        continue;
                    }

                    // BC compressed texture path
                    if let Some(bc_data) = decoded.bc_data {
                        let algo = self.compression_algorithm;
                        let fb = algo.gpu_frame_bytes(decoded.width, decoded.height);
                        let is_essential =
                            (decoded.frame_index as i32 - center).unsigned_abs() <= ESSENTIAL_RADIUS as u32;

                        if is_essential {
                            while self.cached_bytes + fb > self.gpu_budget_bytes {
                                if self.evict_n_outside(center, 10) == 0 {
                                    break;
                                }
                            }
                        } else if self.cached_bytes + fb > self.gpu_budget_bytes {
                            self.cache_full.store(true, Ordering::Relaxed);
                            continue;
                        }

                        if let Some(gl) = frame.gl() {
                            match create_compressed_gl_texture(
                                gl,
                                algo,
                                decoded.bc_padded_width,
                                decoded.bc_padded_height,
                                &bc_data,
                            ) {
                                Ok(gl_texture) => {
                                    let tex_id = frame.register_native_glow_texture(gl_texture);
                                    self.access_counter += 1;
                                    self.cached_bytes += fb;
                                    self.textures.insert(
                                        decoded.frame_index,
                                        CacheEntry {
                                            texture_handle: None,
                                            native_texture_id: Some(tex_id),
                                            gl_texture: Some(gl_texture),
                                            last_access: self.access_counter,
                                            bytes: fb,
                                        },
                                    );
                                    self.gpu_cached.write().unwrap().insert(decoded.frame_index);
                                    uploaded_any = true;
                                }
                                Err(e) => {
                                    log::error!("Failed to create compressed GL texture for frame {}: {}", decoded.frame_index, e);
                                }
                            }
                        }
                        continue;
                    }

                    // Standard RGBA texture path
                    let color_image = match decoded.color_image {
                        Some(img) => img,
                        None => continue,
                    };

                    let fb = decoded.width as u64 * decoded.height as u64 * 4;
                    let is_essential =
                        (decoded.frame_index as i32 - center).unsigned_abs() <= ESSENTIAL_RADIUS as u32;

                    if is_essential {
                        while self.cached_bytes + fb > self.gpu_budget_bytes {
                            if self.evict_n_outside(center, 10) == 0 {
                                break;
                            }
                        }
                    } else {
                        if self.cached_bytes + fb > self.gpu_budget_bytes {
                            self.cache_full.store(true, Ordering::Relaxed);
                            continue;
                        }
                    }

                    let texture = ctx.load_texture(
                        format!("frame_{}", decoded.frame_index),
                        color_image,
                        egui::TextureOptions::LINEAR,
                    );

                    self.access_counter += 1;
                    self.cached_bytes += fb;
                    self.textures.insert(
                        decoded.frame_index,
                        CacheEntry {
                            texture_handle: Some(texture),
                            native_texture_id: None,
                            gl_texture: None,
                            last_access: self.access_counter,
                            bytes: fb,
                        },
                    );
                    self.gpu_cached.write().unwrap().insert(decoded.frame_index);
                    uploaded_any = true;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }

        // Put the receiver back (unless disconnected)
        if !disconnected {
            self.decoded_rx = Some(rx);
        }

        uploaded_any || hit_budget
    }

    /// Evict up to N LRU frames that are outside the essential zone (±15 from center).
    /// Returns number of frames evicted.
    fn evict_n_outside(&mut self, center: i32, count: usize) -> usize {
        // Collect candidates sorted by LRU
        let mut candidates: Vec<(u32, u64)> = self
            .textures
            .iter()
            .filter(|(&idx, _)| (idx as i32 - center).unsigned_abs() > ESSENTIAL_RADIUS as u32)
            .map(|(&idx, entry)| (idx, entry.last_access))
            .collect();
        candidates.sort_by_key(|&(_, access)| access);

        let mut evicted = 0;
        let mut gpu_cached = self.gpu_cached.write().unwrap();
        for (key, _) in candidates.into_iter().take(count) {
            if let Some(entry) = self.textures.remove(&key) {
                self.cached_bytes = self.cached_bytes.saturating_sub(entry.bytes);
                // For native textures, delete the GL texture directly
                if let Some(gl_tex) = entry.gl_texture {
                    if let Some(gl) = &self.gl_context {
                        use glow::HasContext;
                        unsafe { gl.delete_texture(gl_tex) };
                    }
                }
                // For managed textures, dropping TextureHandle handles cleanup
            }
            gpu_cached.remove(&key);
            evicted += 1;
        }
        evicted
    }

    /// Free all native (BC) GL textures directly.
    ///
    /// Native textures registered via `register_native_glow_texture` are NOT tracked
    /// by egui's TextureManager (only the painter knows about them). Calling
    /// `tex_manager().free()` on a User texture is a silent no-op because the ID
    /// isn't in the TextureManager's `metas` HashMap. We must call
    /// `gl.delete_texture()` directly to actually free VRAM.
    fn free_all_native_textures(&mut self) {
        use glow::HasContext;
        if let Some(gl) = &self.gl_context {
            for entry in self.textures.values() {
                if let Some(gl_tex) = entry.gl_texture {
                    unsafe { gl.delete_texture(gl_tex) };
                }
            }
        }
    }

    /// Returns true if a prefetch thread is active (frames may arrive).
    pub fn is_prefetch_active(&self) -> bool {
        self.decoded_rx.is_some()
    }

    /// Check if a frame is currently cached.
    pub fn is_cached(&self, frame_index: u32) -> bool {
        self.textures.contains_key(&frame_index)
    }

    /// Number of frames currently cached in GPU memory.
    pub fn cached_count(&self) -> u32 {
        self.textures.len() as u32
    }

    /// Current frame dimensions used by the cache.
    pub fn frame_dimensions(&self) -> (u32, u32) {
        (self.frame_width, self.frame_height)
    }

    /// Check if a frame's file was not found on disk.
    pub fn is_missing(&self, frame_index: u32) -> bool {
        self.missing_frames.contains(&frame_index)
    }

    /// Shut down the prefetch thread and clear all textures.
    pub fn clear(&mut self) {
        self.shutdown_prefetch();
        self.free_all_native_textures();
        self.textures.clear();
        self.missing_frames.clear();
        self.gpu_cached.write().unwrap().clear();
        self.cached_bytes = 0;
        self.frame_dir = PathBuf::new();
        self.total_frames = 0;
    }

    fn start_prefetch(&mut self) {
        // Bounded channel: limits decoded frames waiting in RAM.
        // 16 slots at ~8 MB each (1080p RGBA) ≈ 128 MB max in-flight.
        let (tx, rx) = mpsc::sync_channel::<DecodedFrame>(16);
        self.decoded_rx = Some(rx);

        let running = Arc::new(AtomicBool::new(true));
        self.running = running.clone();

        let current_frame = self.current_frame.clone();
        let cache_full = self.cache_full.clone();
        let gpu_cached = self.gpu_cached.clone();
        let frame_dir = self.frame_dir.clone();
        let total_frames = self.total_frames;
        let frame_width = self.frame_width;
        let frame_height = self.frame_height;
        let algo = self.compression_algorithm;
        let storage_mode = self.cache_storage_mode;

        let handle = std::thread::Builder::new()
            .name("frame-prefetch".into())
            .spawn(move || {
                Self::prefetch_loop(
                    running,
                    current_frame,
                    cache_full,
                    gpu_cached,
                    frame_dir,
                    total_frames,
                    frame_width,
                    frame_height,
                    tx,
                    algo,
                    storage_mode,
                );
            })
            .expect("Failed to spawn prefetch thread");

        self.prefetch_handle = Some(handle);
    }

    fn prefetch_loop(
        running: Arc<AtomicBool>,
        current_frame: Arc<AtomicI32>,
        cache_full: Arc<AtomicBool>,
        gpu_cached: Arc<RwLock<HashSet<u32>>>,
        frame_dir: PathBuf,
        total_frames: u32,
        frame_width: u32,
        frame_height: u32,
        tx: mpsc::SyncSender<DecodedFrame>,
        algo: CompressionAlgorithm,
        storage_mode: CacheStorageMode,
    ) {
        let num_workers = std::thread::available_parallelism()
            .map(|n| (n.get() * 3 / 4).max(2))
            .unwrap_or(2)
            .min(12);

        let mut last_center: i32 = -1;
        let mut sent: HashSet<u32> = HashSet::new();
        // Set to true after essential frames are decoded so we can keep loading
        // forward frames without waiting for center to change.
        let mut forward_pending = false;

        while running.load(Ordering::Relaxed) {
            let center = current_frame.load(Ordering::Relaxed);
            let center_changed = center != last_center;

            if center_changed {
                last_center = center;
                forward_pending = false;
                let was_full = cache_full.load(Ordering::Relaxed);
                cache_full.store(false, Ordering::Relaxed);

                let ess_start = (center - ESSENTIAL_RADIUS).max(0) as u32;
                let ess_end =
                    ((center + ESSENTIAL_RADIUS) as u32).min(total_frames.saturating_sub(1));

                // Build essential list while holding read lock briefly (no clone)
                let essential = {
                    let cached = gpu_cached.read().unwrap();

                    if was_full {
                        sent.retain(|idx| cached.contains(idx));
                    } else {
                        let purge_before = (center - ESSENTIAL_RADIUS).max(0) as u32;
                        sent.retain(|&idx| idx >= purge_before);
                    }

                    let mut ess: Vec<u32> = (ess_start..=ess_end)
                        .filter(|idx| !sent.contains(idx) && !cached.contains(idx))
                        .collect();
                    ess.sort_by_key(|&i| (i as i32 - center).unsigned_abs());
                    ess
                };

                if !essential.is_empty() {
                    let aborted = Self::decode_batch_parallel(
                        &essential,
                        &frame_dir,
                        frame_width,
                        frame_height,
                        &tx,
                        &running,
                        &current_frame,
                        center,
                        &mut sent,
                        num_workers,
                        algo,
                        storage_mode,
                    );
                    if aborted {
                        continue;
                    }
                }

                forward_pending = true;
            }

            // Greedy forward loading: keep fetching batches until cache is full,
            // cursor moves, or all frames are sent.
            if forward_pending && !cache_full.load(Ordering::Relaxed) {
                let ess_end = ((center + ESSENTIAL_RADIUS) as u32)
                    .min(total_frames.saturating_sub(1));
                let fwd_start = (ess_end + 1).min(total_frames);

                let forward = {
                    let cached = gpu_cached.read().unwrap();
                    (fwd_start..total_frames)
                        .filter(|idx| !sent.contains(idx) && !cached.contains(idx))
                        .take(num_workers * 8)
                        .collect::<Vec<u32>>()
                };

                if forward.is_empty() {
                    forward_pending = false;
                } else {
                    Self::decode_batch_parallel_with_full_check(
                        &forward,
                        &frame_dir,
                        frame_width,
                        frame_height,
                        &tx,
                        &running,
                        &current_frame,
                        &cache_full,
                        center,
                        &mut sent,
                        num_workers,
                        algo,
                        storage_mode,
                    );
                    // If cache became full or cursor moved, stop forward loading
                    if cache_full.load(Ordering::Relaxed)
                        || current_frame.load(Ordering::Relaxed) != center
                    {
                        forward_pending = false;
                    }
                    // Don't sleep — immediately check for more forward work
                    continue;
                }
            }

            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// Decode frames in parallel batches. Returns true if aborted (cursor moved or shutdown).
    fn decode_batch_parallel(
        indices: &[u32],
        frame_dir: &Path,
        frame_width: u32,
        frame_height: u32,
        tx: &mpsc::SyncSender<DecodedFrame>,
        running: &AtomicBool,
        current_frame: &AtomicI32,
        expected_center: i32,
        sent: &mut HashSet<u32>,
        num_workers: usize,
        algo: CompressionAlgorithm,
        storage_mode: CacheStorageMode,
    ) -> bool {
        for chunk in indices.chunks(num_workers) {
            if !running.load(Ordering::Relaxed) {
                return true;
            }
            if current_frame.load(Ordering::Relaxed) != expected_center {
                return true;
            }

            let results: Vec<DecodedFrame> = std::thread::scope(|s| {
                let handles: Vec<_> = chunk
                    .iter()
                    .map(|&idx| {
                        let dir = frame_dir;
                        s.spawn(move || Self::decode_one(dir, idx, frame_width, frame_height, algo, storage_mode))
                    })
                    .collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect()
            });

            for decoded in results {
                sent.insert(decoded.frame_index);
                if tx.send(decoded).is_err() {
                    return true;
                }
            }
        }
        false
    }

    /// Like decode_batch_parallel but also checks cache_full between batches.
    fn decode_batch_parallel_with_full_check(
        indices: &[u32],
        frame_dir: &Path,
        frame_width: u32,
        frame_height: u32,
        tx: &mpsc::SyncSender<DecodedFrame>,
        running: &AtomicBool,
        current_frame: &AtomicI32,
        cache_full: &AtomicBool,
        expected_center: i32,
        sent: &mut HashSet<u32>,
        num_workers: usize,
        algo: CompressionAlgorithm,
        storage_mode: CacheStorageMode,
    ) {
        for chunk in indices.chunks(num_workers) {
            if !running.load(Ordering::Relaxed) {
                return;
            }
            if current_frame.load(Ordering::Relaxed) != expected_center {
                return;
            }
            if cache_full.load(Ordering::Relaxed) {
                return;
            }

            let results: Vec<DecodedFrame> = std::thread::scope(|s| {
                let handles: Vec<_> = chunk
                    .iter()
                    .map(|&idx| {
                        let dir = frame_dir;
                        s.spawn(move || Self::decode_one(dir, idx, frame_width, frame_height, algo, storage_mode))
                    })
                    .collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect()
            });

            for decoded in results {
                sent.insert(decoded.frame_index);
                if tx.send(decoded).is_err() {
                    return;
                }
            }
        }
    }

    /// Decode a single frame from disk. Tries raw first if storage mode allows,
    /// then falls back to compressed. Public for use in benchmark.
    pub fn decode_one(
        frame_dir: &Path,
        idx: u32,
        frame_width: u32,
        frame_height: u32,
        algo: CompressionAlgorithm,
        storage_mode: CacheStorageMode,
    ) -> DecodedFrame {
        let fname = format!("frame_{:06}", idx + 1);

        // BC path: read file, strip header, pass through raw BC bytes
        if algo.is_gpu_compressed() {
            let ext = crate::video::compression::file_extension(algo);
            let comp_path = frame_dir.join("compressed").join(format!("{fname}.{ext}"));
            return match std::fs::read(&comp_path) {
                Ok(file_data) => {
                    match crate::video::compression::parse_bc_header(&file_data) {
                        Some((orig_w, orig_h, bc_data)) => {
                            let padded_w = (orig_w + 3) & !3;
                            let padded_h = (orig_h + 3) & !3;
                            DecodedFrame {
                                frame_index: idx,
                                color_image: None,
                                bc_data: Some(bc_data.to_vec()),
                                bc_padded_width: padded_w,
                                bc_padded_height: padded_h,
                                width: orig_w,
                                height: orig_h,
                                missing: false,
                            }
                        }
                        None => Self::missing_frame(idx, frame_width, frame_height),
                    }
                }
                Err(_) => Self::missing_frame(idx, frame_width, frame_height),
            };
        }

        let expected_len = frame_width as usize * frame_height as usize * 4;

        // Try raw first if storage mode allows
        if storage_mode.load_raw_first() {
            let raw_path = frame_dir.join("raw").join(format!("{fname}.raw"));
            if let Some(decoded) = Self::decode_raw_frame(&raw_path, idx, frame_width, frame_height) {
                return decoded;
            }
        }

        // Try compressed if storage mode allows
        if storage_mode.load_compressed() {
            let ext = crate::video::compression::file_extension(algo);
            let comp_path = frame_dir.join("compressed").join(format!("{fname}.{ext}"));
            if let Some(decoded) = Self::decode_compressed_frame(
                &comp_path, idx, frame_width, frame_height, algo, expected_len,
            ) {
                return decoded;
            }
        }

        Self::missing_frame(idx, frame_width, frame_height)
    }

    fn missing_frame(idx: u32, frame_width: u32, frame_height: u32) -> DecodedFrame {
        DecodedFrame {
            frame_index: idx,
            color_image: None,
            bc_data: None,
            bc_padded_width: 0,
            bc_padded_height: 0,
            width: frame_width,
            height: frame_height,
            missing: true,
        }
    }

    /// Read an uncompressed .raw frame file directly into a ColorImage.
    fn decode_raw_frame(path: &Path, frame_index: u32, frame_width: u32, frame_height: u32) -> Option<DecodedFrame> {
        let rgba = std::fs::read(path).ok()?;

        let expected = frame_width as usize * frame_height as usize * 4;
        if rgba.len() != expected {
            return None;
        }

        let color_image = egui::ColorImage::from_rgba_unmultiplied(
            [frame_width as usize, frame_height as usize],
            &rgba,
        );

        Some(DecodedFrame {
            frame_index,
            color_image: Some(color_image),
            bc_data: None,
            bc_padded_width: 0,
            bc_padded_height: 0,
            width: frame_width,
            height: frame_height,
            missing: false,
        })
    }

    /// Read and decompress a compressed frame file using the configured algorithm.
    fn decode_compressed_frame(
        path: &Path,
        frame_index: u32,
        frame_width: u32,
        frame_height: u32,
        algo: CompressionAlgorithm,
        expected_len: usize,
    ) -> Option<DecodedFrame> {
        let compressed = std::fs::read(path).ok()?;
        let rgba = crate::video::compression::decompress(&compressed, algo, expected_len).ok()?;

        if rgba.len() != expected_len {
            return None;
        }

        let color_image = egui::ColorImage::from_rgba_unmultiplied(
            [frame_width as usize, frame_height as usize],
            &rgba,
        );

        Some(DecodedFrame {
            frame_index,
            color_image: Some(color_image),
            bc_data: None,
            bc_padded_width: 0,
            bc_padded_height: 0,
            width: frame_width,
            height: frame_height,
            missing: false,
        })
    }

    fn shutdown_prefetch(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        self.decoded_rx = None; // Drop receiver to unblock sender
        if let Some(handle) = self.prefetch_handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for FrameCache {
    fn drop(&mut self) {
        self.shutdown_prefetch();
    }
}

/// Create a compressed GL texture from BC data using glow.
fn create_compressed_gl_texture(
    gl: &std::sync::Arc<glow::Context>,
    algo: CompressionAlgorithm,
    width: u32,
    height: u32,
    data: &[u8],
) -> Result<glow::Texture, String> {
    use glow::HasContext;

    let internal_format = match algo {
        CompressionAlgorithm::Bc1 => GL_COMPRESSED_RGBA_S3TC_DXT1_EXT,
        CompressionAlgorithm::Bc7 => GL_COMPRESSED_RGBA_BPTC_UNORM,
        _ => return Err("Not a BC format".into()),
    };

    unsafe {
        let texture = gl.create_texture().map_err(|e| format!("glCreateTexture: {e}"))?;
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));

        gl.compressed_tex_image_2d(
            glow::TEXTURE_2D,
            0,
            internal_format,
            width as i32,
            height as i32,
            0,
            data.len() as i32,
            data,
        );

        // Check for GL error
        let err = gl.get_error();
        if err != glow::NO_ERROR {
            gl.delete_texture(texture);
            return Err(format!("glCompressedTexImage2D error: 0x{:X}", err));
        }

        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);

        gl.bind_texture(glow::TEXTURE_2D, None);

        Ok(texture)
    }
}

/// Check if the GL context supports the required compressed texture extensions.
pub fn check_bc_support(gl: &glow::Context) -> (bool, bool) {
    use glow::HasContext;
    let extensions = gl.supported_extensions();
    let bc1_ok = extensions.contains("GL_EXT_texture_compression_s3tc")
        || extensions.contains("GL_S3_s3tc");
    let bc7_ok = extensions.contains("GL_ARB_texture_compression_bptc");
    (bc1_ok, bc7_ok)
}

// --- Disk cache management ---

/// Estimate disk space needed for a video's cache.
pub fn estimate_cache_bytes(
    total_frames: u32,
    frame_width: u32,
    frame_height: u32,
    storage_mode: CacheStorageMode,
    algo: CompressionAlgorithm,
) -> u64 {
    if algo.is_gpu_compressed() {
        // BC formats: exact sizes, +8 bytes for header per frame
        let bc_per_frame = algo.gpu_frame_bytes(frame_width, frame_height) + 8;
        return total_frames as u64 * bc_per_frame;
    }

    let raw_per_frame = frame_width as u64 * frame_height as u64 * 4;
    match storage_mode {
        CacheStorageMode::CompressedOnly => total_frames as u64 * raw_per_frame * 3 / 10,
        CacheStorageMode::RawOnly => total_frames as u64 * raw_per_frame,
        CacheStorageMode::Both => total_frames as u64 * raw_per_frame * 13 / 10,
    }
}

/// Proactively evict caches before extraction starts so the new video will fit.
/// Evicts until (current_cache_size + estimated_new) <= disk_limit * 110%.
/// Uses two-tier eviction: expendable subdirs first, then entire video dirs.
/// Never evicts the directory identified by `current_sha`.
pub fn evict_for_new_video(
    cache_root: &Path,
    max_size_mb: u32,
    current_sha: &str,
    estimated_new_bytes: u64,
    storage_mode: CacheStorageMode,
) {
    let budget = max_size_mb as u64 * 1024 * 1024 * 11 / 10; // 110% of limit

    let entries = match std::fs::read_dir(cache_root) {
        Ok(e) => e,
        Err(_) => return,
    };

    struct CacheDir {
        path: PathBuf,
        name: String,
        expendable_size: u64,
        expendable_subdir: &'static str,
        access_time: u64,
    }

    // Determine which subdir is expendable based on storage mode
    let expendable_name = match storage_mode {
        CacheStorageMode::Both | CacheStorageMode::CompressedOnly => "raw",
        CacheStorageMode::RawOnly => "compressed",
    };

    let mut dirs: Vec<CacheDir> = Vec::new();
    let mut total_size = 0u64;

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let size = get_dir_size(&path);
        let expendable_dir = path.join(expendable_name);
        let expendable_size = if expendable_dir.is_dir() { get_dir_size(&expendable_dir) } else { 0 };
        let access_time = read_cache_access_time(&path);
        total_size += size;
        dirs.push(CacheDir { path, name, expendable_size, expendable_subdir: expendable_name, access_time });
    }

    if total_size + estimated_new_bytes <= budget {
        return;
    }

    log::info!(
        "Pre-extraction eviction: current cache {:.0} MB + estimated new {:.0} MB > budget {:.0} MB",
        total_size as f64 / (1024.0 * 1024.0),
        estimated_new_bytes as f64 / (1024.0 * 1024.0),
        budget as f64 / (1024.0 * 1024.0),
    );

    // Sort by access time ascending (oldest first) for LRU eviction
    dirs.sort_by_key(|d| d.access_time);

    // Phase 1: Remove expendable subdirs from non-current caches
    for dir in &dirs {
        if total_size + estimated_new_bytes <= budget {
            break;
        }
        if dir.name == current_sha || dir.expendable_size == 0 {
            continue;
        }
        let sub_dir = dir.path.join(dir.expendable_subdir);
        log::info!("Evicting {} cache: {} ({} MB)", dir.expendable_subdir, dir.name, dir.expendable_size / (1024 * 1024));
        if let Err(e) = std::fs::remove_dir_all(&sub_dir) {
            log::warn!("Failed to remove {} cache dir {}: {e}", dir.expendable_subdir, sub_dir.display());
        } else {
            total_size -= dir.expendable_size;
        }
    }

    if total_size + estimated_new_bytes <= budget {
        return;
    }

    // Phase 2: Remove entire video cache directories
    for dir in &dirs {
        if total_size + estimated_new_bytes <= budget {
            break;
        }
        if dir.name == current_sha {
            continue;
        }
        let remaining = get_dir_size(&dir.path);
        log::info!("Evicting cache directory: {} ({} MB)", dir.name, remaining / (1024 * 1024));
        if let Err(e) = std::fs::remove_dir_all(&dir.path) {
            log::warn!("Failed to remove cache dir {}: {e}", dir.path.display());
        } else {
            total_size -= remaining;
        }
    }
}

/// Recursively compute the total size of a directory in bytes.
pub fn get_dir_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                total += get_dir_size(&path);
            } else if let Ok(meta) = entry.metadata() {
                total += meta.len();
            }
        }
    }
    total
}

/// Touch a `.last_accessed` marker file inside a video's cache directory
/// so we can determine LRU order for eviction.
pub fn touch_cache_access(video_cache_dir: &Path) {
    let marker = video_cache_dir.join(".last_accessed");
    let _ = std::fs::write(&marker, format!("{}", std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()));
}

/// Read the LRU timestamp from a video cache directory.
/// Returns 0 if no marker exists (treat as oldest).
fn read_cache_access_time(video_cache_dir: &Path) -> u64 {
    let marker = video_cache_dir.join(".last_accessed");
    std::fs::read_to_string(&marker)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Enforce disk cache size limit with two-tier eviction:
/// 1. First remove expendable subdirs from non-current video caches (LRU order)
/// 2. If still over budget, remove entire video cache directories (LRU order)
/// The directory identified by `current_sha` is never evicted.
///
/// Returns `Err` if the current video alone exceeds the limit.
pub fn manage_disk_cache(
    cache_root: &Path,
    max_size_mb: u32,
    current_sha: &str,
    storage_mode: CacheStorageMode,
) -> Result<(), String> {
    let max_bytes = max_size_mb as u64 * 1024 * 1024;

    // Enumerate video cache subdirectories with their sizes and access times
    let entries = match std::fs::read_dir(cache_root) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };

    let expendable_name = match storage_mode {
        CacheStorageMode::Both | CacheStorageMode::CompressedOnly => "raw",
        CacheStorageMode::RawOnly => "compressed",
    };

    struct CacheDir {
        path: PathBuf,
        name: String,
        expendable_size: u64,
        expendable_subdir: &'static str,
        access_time: u64,
    }

    let mut dirs: Vec<CacheDir> = Vec::new();
    let mut total_size = 0u64;

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let total = get_dir_size(&path);
        let expendable_dir = path.join(expendable_name);
        let expendable_size = if expendable_dir.is_dir() { get_dir_size(&expendable_dir) } else { 0 };
        let access_time = read_cache_access_time(&path);
        total_size += total;
        dirs.push(CacheDir { path, name, expendable_size, expendable_subdir: expendable_name, access_time });
    }

    if total_size <= max_bytes {
        return Ok(());
    }

    // Sort by access time ascending (oldest first) for LRU eviction
    dirs.sort_by_key(|d| d.access_time);

    // Phase 1: Remove expendable subdirs from non-current caches
    for dir in &dirs {
        if total_size <= max_bytes {
            break;
        }
        if dir.name == current_sha || dir.expendable_size == 0 {
            continue;
        }
        let sub_dir = dir.path.join(dir.expendable_subdir);
        log::info!("Evicting {} cache: {} ({} MB)", dir.expendable_subdir, dir.name, dir.expendable_size / (1024 * 1024));
        if let Err(e) = std::fs::remove_dir_all(&sub_dir) {
            log::warn!("Failed to remove {} cache dir {}: {e}", dir.expendable_subdir, sub_dir.display());
        } else {
            total_size -= dir.expendable_size;
        }
    }

    if total_size <= max_bytes {
        return Ok(());
    }

    // Phase 2: Remove entire video cache directories
    for dir in &dirs {
        if total_size <= max_bytes {
            break;
        }
        if dir.name == current_sha {
            continue;
        }
        let remaining = get_dir_size(&dir.path);
        log::info!("Evicting cache directory: {} ({} MB)", dir.name, remaining / (1024 * 1024));
        if let Err(e) = std::fs::remove_dir_all(&dir.path) {
            log::warn!("Failed to remove cache dir {}: {e}", dir.path.display());
        } else {
            total_size -= remaining;
        }
    }

    // Check if still over limit (means current video alone is too large)
    if total_size > max_bytes {
        let current_size_mb = total_size / (1024 * 1024);
        return Err(format!(
            "Video cache ({current_size_mb} MB) exceeds disk cache limit ({max_size_mb} MB). \
             Increase the disk cache size in Settings."
        ));
    }

    Ok(())
}

/// Decompress all compressed frames in a cache directory to raw files.
/// Spawns a background thread. The cancel flag can be set to abort.
/// Returns a handle to the decompression thread.
pub fn decompress_cache_async(
    cache_dir: PathBuf,
    cancel: Arc<AtomicBool>,
    algo: CompressionAlgorithm,
    frame_width: u32,
    frame_height: u32,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("cache-decompress".into())
        .spawn(move || {
            // BC formats are GPU-native; no decompression to raw needed
            if algo.is_gpu_compressed() {
                return;
            }

            let compressed_dir = cache_dir.join("compressed");
            let raw_dir = cache_dir.join("raw");
            let ext = crate::video::compression::file_extension(algo);
            let expected_len = frame_width as usize * frame_height as usize * 4;

            if !compressed_dir.is_dir() {
                return;
            }
            if let Err(e) = std::fs::create_dir_all(&raw_dir) {
                log::warn!("Failed to create raw cache dir: {e}");
                return;
            }

            // Collect compressed files to decompress
            let mut comp_files: Vec<PathBuf> = Vec::new();
            if let Ok(entries) = std::fs::read_dir(&compressed_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().is_some_and(|e| e == ext) {
                        comp_files.push(path);
                    }
                }
            }
            comp_files.sort();

            let num_workers = std::thread::available_parallelism()
                .map(|n| (n.get() * 3 / 4).max(2))
                .unwrap_or(2)
                .min(12);

            log::info!(
                "Decompressing {} frames to raw cache ({} workers, algo={})",
                comp_files.len(),
                num_workers,
                algo.label(),
            );

            let mut decompressed = 0u32;
            for chunk in comp_files.chunks(num_workers) {
                if cancel.load(Ordering::Relaxed) {
                    log::info!("Raw cache decompression cancelled");
                    return;
                }

                std::thread::scope(|s| {
                    let handles: Vec<_> = chunk
                        .iter()
                        .map(|comp_path| {
                            let raw_d = &raw_dir;
                            s.spawn(move || {
                                let stem = comp_path.file_stem().unwrap_or_default();
                                let raw_path = raw_d.join(format!("{}.raw", stem.to_string_lossy()));

                                // Skip if already decompressed
                                if raw_path.exists() {
                                    return;
                                }

                                if let Ok(compressed_data) = std::fs::read(comp_path) {
                                    if let Ok(raw) = crate::video::compression::decompress(
                                        &compressed_data, algo, expected_len,
                                    ) {
                                        let _ = std::fs::write(&raw_path, &raw);
                                    }
                                }
                            })
                        })
                        .collect();
                    for h in handles {
                        let _ = h.join();
                    }
                });

                decompressed += chunk.len() as u32;
            }

            log::info!("Raw cache decompression complete: {} frames", decompressed);
        })
        .expect("Failed to spawn decompression thread")
}

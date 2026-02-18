use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;

/// Number of frames around the cursor that are considered essential.
/// Essential frames can trigger LRU eviction of non-essential frames.
const ESSENTIAL_RADIUS: i32 = 15;

/// A decoded frame ready to be turned into a GPU texture on the main thread.
/// The ColorImage is created on the background thread to avoid blocking the UI.
pub struct DecodedFrame {
    pub frame_index: u32,
    pub color_image: Option<egui::ColorImage>,
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
}

struct CacheEntry {
    texture: egui::TextureHandle,
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
        }
    }

    /// Set GPU memory budget and frame dimensions for budget calculations.
    pub fn set_gpu_budget(&mut self, gpu_cache_mb: u32, frame_width: u32, frame_height: u32) {
        self.gpu_budget_bytes = gpu_cache_mb as u64 * 1024 * 1024;
        self.frame_bytes = frame_width as u64 * frame_height as u64 * 4;
    }

    /// Set the source directory, total frame count, and frame dimensions.
    /// Clears existing cache and restarts prefetch.
    pub fn set_source(&mut self, frame_dir: &Path, total_frames: u32, frame_width: u32, frame_height: u32) {
        self.shutdown_prefetch();
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

        if total_frames > 0 && frame_dir.is_dir() {
            self.start_prefetch();
        }
    }

    /// Get the texture for a specific frame. Returns a cloned handle (cheap Arc clone).
    /// Returns None if not yet loaded.
    pub fn get_frame(&mut self, frame_index: u32) -> Option<egui::TextureHandle> {
        if let Some(entry) = self.textures.get_mut(&frame_index) {
            self.access_counter += 1;
            entry.last_access = self.access_counter;
            return Some(entry.texture.clone());
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
    pub fn process_uploads(&mut self, ctx: &egui::Context, budget_ms: f64) -> bool {
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

                    let color_image = match decoded.color_image {
                        Some(img) => img,
                        None => continue,
                    };

                    let fb = decoded.width as u64 * decoded.height as u64 * 4;
                    let is_essential =
                        (decoded.frame_index as i32 - center).unsigned_abs() <= ESSENTIAL_RADIUS as u32;

                    if is_essential {
                        // Essential: evict LRU frames outside ±15 (10 at a time) until there's room
                        while self.cached_bytes + fb > self.gpu_budget_bytes {
                            if self.evict_n_outside(center, 10) == 0 {
                                break; // Nothing left to evict
                            }
                        }
                    } else {
                        // Non-essential: only cache if room in budget
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
                            texture,
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
            }
            gpu_cached.remove(&key);
            evicted += 1;
        }
        evicted
    }

    /// Returns true if a prefetch thread is active (frames may arrive).
    pub fn is_prefetch_active(&self) -> bool {
        self.decoded_rx.is_some()
    }

    /// Check if a frame is currently cached.
    pub fn is_cached(&self, frame_index: u32) -> bool {
        self.textures.contains_key(&frame_index)
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
                        s.spawn(move || Self::decode_one(dir, idx, frame_width, frame_height))
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
                        s.spawn(move || Self::decode_one(dir, idx, frame_width, frame_height))
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

    fn decode_one(frame_dir: &Path, idx: u32, frame_width: u32, frame_height: u32) -> DecodedFrame {
        let fname = format!("frame_{:06}", idx + 1);
        let raw_path = frame_dir.join("raw").join(format!("{fname}.raw"));
        let lz4_path = frame_dir.join("lz4").join(format!("{fname}.lz4"));

        // Try uncompressed first (faster, no decompression needed)
        if let Some(decoded) = Self::decode_raw_frame(&raw_path, idx, frame_width, frame_height) {
            return decoded;
        }
        // Fall back to compressed
        Self::decode_lz4_frame(&lz4_path, idx, frame_width, frame_height).unwrap_or(DecodedFrame {
            frame_index: idx,
            color_image: None,
            width: frame_width,
            height: frame_height,
            missing: true,
        })
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
            width: frame_width,
            height: frame_height,
            missing: false,
        })
    }

    /// Read and decompress an LZ4-compressed frame file.
    fn decode_lz4_frame(path: &Path, frame_index: u32, frame_width: u32, frame_height: u32) -> Option<DecodedFrame> {
        let compressed = std::fs::read(path).ok()?;
        let rgba = lz4_flex::decompress_size_prepended(&compressed).ok()?;

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

// --- Disk cache management ---

/// Estimate disk space needed for a video's cache (both lz4 + raw).
/// Uses a 1.3x multiplier on raw frame size to account for lz4 overhead.
pub fn estimate_cache_bytes(total_frames: u32, frame_width: u32, frame_height: u32) -> u64 {
    let raw_per_frame = frame_width as u64 * frame_height as u64 * 4;
    // raw files + lz4 files (~30% of raw size)
    total_frames as u64 * raw_per_frame * 13 / 10
}

/// Proactively evict caches before extraction starts so the new video will fit.
/// Evicts until (current_cache_size + estimated_new) <= disk_limit * 110%.
/// Uses two-tier eviction: raw/ subdirs first, then entire video dirs.
/// Never evicts the directory identified by `current_sha`.
pub fn evict_for_new_video(
    cache_root: &Path,
    max_size_mb: u32,
    current_sha: &str,
    estimated_new_bytes: u64,
) {
    let budget = max_size_mb as u64 * 1024 * 1024 * 11 / 10; // 110% of limit

    let entries = match std::fs::read_dir(cache_root) {
        Ok(e) => e,
        Err(_) => return,
    };

    struct CacheDir {
        path: PathBuf,
        name: String,
        raw_size: u64,
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
        let size = get_dir_size(&path);
        let raw_dir = path.join("raw");
        let raw_size = if raw_dir.is_dir() { get_dir_size(&raw_dir) } else { 0 };
        let access_time = read_cache_access_time(&path);
        total_size += size;
        dirs.push(CacheDir { path, name, raw_size, access_time });
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

    // Phase 1: Remove raw/ subdirs from non-current caches
    for dir in &dirs {
        if total_size + estimated_new_bytes <= budget {
            break;
        }
        if dir.name == current_sha || dir.raw_size == 0 {
            continue;
        }
        let raw_dir = dir.path.join("raw");
        log::info!("Evicting raw cache: {} ({} MB)", dir.name, dir.raw_size / (1024 * 1024));
        if let Err(e) = std::fs::remove_dir_all(&raw_dir) {
            log::warn!("Failed to remove raw cache dir {}: {e}", raw_dir.display());
        } else {
            total_size -= dir.raw_size;
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
/// 1. First remove `raw/` (uncompressed) subdirs from non-current video caches (LRU order)
/// 2. If still over budget, remove entire video cache directories (LRU order)
/// The directory identified by `current_sha` is never evicted.
///
/// Returns `Err` if the current video alone exceeds the limit.
pub fn manage_disk_cache(
    cache_root: &Path,
    max_size_mb: u32,
    current_sha: &str,
) -> Result<(), String> {
    let max_bytes = max_size_mb as u64 * 1024 * 1024;

    // Enumerate video cache subdirectories with their sizes and access times
    let entries = match std::fs::read_dir(cache_root) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };

    struct CacheDir {
        path: PathBuf,
        name: String,
        raw_size: u64,
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
        let raw_dir = path.join("raw");
        let raw_size = if raw_dir.is_dir() { get_dir_size(&raw_dir) } else { 0 };
        let access_time = read_cache_access_time(&path);
        total_size += total;
        dirs.push(CacheDir { path, name, raw_size, access_time });
    }

    if total_size <= max_bytes {
        return Ok(());
    }

    // Sort by access time ascending (oldest first) for LRU eviction
    dirs.sort_by_key(|d| d.access_time);

    // Phase 1: Remove uncompressed (raw/) subdirs from non-current caches
    for dir in &dirs {
        if total_size <= max_bytes {
            break;
        }
        if dir.name == current_sha || dir.raw_size == 0 {
            continue;
        }
        let raw_dir = dir.path.join("raw");
        log::info!("Evicting raw cache: {} ({} MB)", dir.name, dir.raw_size / (1024 * 1024));
        if let Err(e) = std::fs::remove_dir_all(&raw_dir) {
            log::warn!("Failed to remove raw cache dir {}: {e}", raw_dir.display());
        } else {
            total_size -= dir.raw_size;
        }
    }

    if total_size <= max_bytes {
        return Ok(());
    }

    // Phase 2: Remove entire video cache directories (compressed + everything)
    for dir in &dirs {
        if total_size <= max_bytes {
            break;
        }
        if dir.name == current_sha {
            continue;
        }
        // raw/ may already be removed in phase 1; use remaining size
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

/// Decompress all LZ4 frames in a cache directory to raw files.
/// Spawns a background thread. The cancel flag can be set to abort.
/// Returns a handle to the decompression thread.
pub fn decompress_cache_async(
    cache_dir: PathBuf,
    cancel: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("cache-decompress".into())
        .spawn(move || {
            let lz4_dir = cache_dir.join("lz4");
            let raw_dir = cache_dir.join("raw");

            if !lz4_dir.is_dir() {
                return;
            }
            if let Err(e) = std::fs::create_dir_all(&raw_dir) {
                log::warn!("Failed to create raw cache dir: {e}");
                return;
            }

            // Collect lz4 files to decompress
            let mut lz4_files: Vec<PathBuf> = Vec::new();
            if let Ok(entries) = std::fs::read_dir(&lz4_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().is_some_and(|ext| ext == "lz4") {
                        lz4_files.push(path);
                    }
                }
            }
            lz4_files.sort();

            let num_workers = std::thread::available_parallelism()
                .map(|n| (n.get() * 3 / 4).max(2))
                .unwrap_or(2)
                .min(12);

            log::info!(
                "Decompressing {} frames to raw cache ({} workers)",
                lz4_files.len(),
                num_workers,
            );

            let mut decompressed = 0u32;
            for chunk in lz4_files.chunks(num_workers) {
                if cancel.load(Ordering::Relaxed) {
                    log::info!("Raw cache decompression cancelled");
                    return;
                }

                std::thread::scope(|s| {
                    let handles: Vec<_> = chunk
                        .iter()
                        .map(|lz4_path| {
                            let raw_d = &raw_dir;
                            s.spawn(move || {
                                let stem = lz4_path.file_stem().unwrap_or_default();
                                let raw_path = raw_d.join(format!("{}.raw", stem.to_string_lossy()));

                                // Skip if already decompressed
                                if raw_path.exists() {
                                    return;
                                }

                                if let Ok(compressed) = std::fs::read(lz4_path) {
                                    if let Ok(raw) = lz4_flex::decompress_size_prepended(&compressed) {
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

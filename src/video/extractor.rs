use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

/// Info about a completed extraction, read from the cache marker file.
#[derive(Debug, Clone)]
pub struct ExtractionInfo {
    pub frame_count: u32,
    pub frame_width: u32,
    pub frame_height: u32,
}

/// Progress update from frame extraction.
#[derive(Debug, Clone)]
pub struct ExtractionProgress {
    pub total_frames: u32,
    pub extracted_frames: u32,
    pub complete: bool,
    pub failed: bool,
    pub error_message: String,
    pub warning_message: String,
}

impl ExtractionProgress {
    pub fn fraction(&self) -> f32 {
        if self.total_frames == 0 {
            0.0
        } else {
            self.extracted_frames as f32 / self.total_frames as f32
        }
    }
}

/// Extracts frames from an MP4 video using ffmpeg CLI.
/// Outputs raw RGBA data piped from ffmpeg, LZ4-compressed per frame in parallel, for fast loading.
pub struct FrameExtractor {
    ffmpeg_path: String,
}

impl FrameExtractor {
    pub fn new(ffmpeg_path: &str) -> Self {
        Self {
            ffmpeg_path: ffmpeg_path.to_string(),
        }
    }

    /// Check if frames were already extracted in the current format.
    /// Returns extraction info if completed, None if extraction needs to run.
    /// Only recognizes v3 format (with correct dimensions); older caches are re-extracted.
    pub fn is_already_extracted(output_dir: &Path) -> Option<ExtractionInfo> {
        let marker = output_dir.join(".complete");
        let content = std::fs::read_to_string(&marker).ok()?;
        let trimmed = content.trim();
        // Current format: "v3:COUNT:WIDTH:HEIGHT"
        let rest = trimmed.strip_prefix("v3:")?;
        let parts: Vec<&str> = rest.split(':').collect();
        if parts.len() == 3 {
            Some(ExtractionInfo {
                frame_count: parts[0].parse().ok()?,
                frame_width: parts[1].parse().ok()?,
                frame_height: parts[2].parse().ok()?,
            })
        } else {
            None
        }
    }

    /// Check if uncompressed raw cache exists for a video cache directory.
    pub fn has_raw_cache(output_dir: &Path) -> bool {
        let raw_dir = output_dir.join("raw");
        raw_dir.is_dir() && std::fs::read_dir(&raw_dir).is_ok_and(|mut d| d.next().is_some())
    }

    /// Get the path for a specific frame file (lz4 subdir).
    pub fn frame_path(output_dir: &Path, frame_index: u32) -> PathBuf {
        output_dir.join("lz4").join(format!("frame_{:06}.lz4", frame_index + 1))
    }

    /// Start frame extraction in a background thread.
    /// FFmpeg decodes the video to raw RGBA which is LZ4-compressed in parallel and saved per frame.
    /// Returns a receiver for progress updates and a cancel flag.
    pub fn extract_async(
        &self,
        video_path: PathBuf,
        output_dir: PathBuf,
        total_frames: u32,
        frame_width: u32,
        frame_height: u32,
    ) -> (mpsc::Receiver<ExtractionProgress>, Arc<AtomicBool>) {
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_clone = cancel.clone();
        let ffmpeg = self.ffmpeg_path.clone();

        std::thread::Builder::new()
            .name("frame-extractor".into())
            .spawn(move || {
                Self::extract_worker(
                    ffmpeg,
                    video_path,
                    output_dir,
                    total_frames,
                    frame_width,
                    frame_height,
                    tx,
                    cancel_clone,
                );
            })
            .expect("Failed to spawn extraction thread");

        (rx, cancel)
    }

    /// Derive ffprobe path from the ffmpeg path.
    fn derive_ffprobe_path(ffmpeg_path: &str) -> String {
        let path = Path::new(ffmpeg_path);
        if let Some(filename) = path.file_name().and_then(|f| f.to_str()) {
            let probe_name = filename.replace("ffmpeg", "ffprobe");
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    return parent.join(&probe_name).to_string_lossy().to_string();
                }
            }
            return probe_name;
        }
        ffmpeg_path.replace("ffmpeg", "ffprobe")
    }

    /// Probe actual video stream dimensions and rotation using ffprobe.
    /// Returns display dimensions (after rotation is applied by FFmpeg's autorotate).
    fn probe_dimensions(ffmpeg_path: &str, video_path: &Path) -> Option<(u32, u32)> {
        let ffprobe_path = Self::derive_ffprobe_path(ffmpeg_path);
        let output = Command::new(&ffprobe_path)
            .args([
                "-v",
                "quiet",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=width,height",
                "-show_entries",
                "stream_tags=rotate",
                "-show_entries",
                "stream_side_data",
                "-of",
                "json",
            ])
            .arg(video_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()?;

        let json: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
        let stream = json.get("streams")?.as_array()?.first()?;

        let coded_w = stream.get("width")?.as_u64()? as u32;
        let coded_h = stream.get("height")?.as_u64()? as u32;

        // Detect rotation from tags (older FFmpeg) or side_data_list (newer FFmpeg)
        let mut rotation: i32 = 0;

        // Check tags.rotate
        if let Some(rot_str) = stream
            .get("tags")
            .and_then(|t| t.get("rotate"))
            .and_then(|v| v.as_str())
        {
            rotation = rot_str.parse().unwrap_or(0);
        }

        // Check side_data_list[].rotation (newer FFmpeg uses display matrix)
        if rotation == 0 {
            if let Some(side_data) = stream.get("side_data_list").and_then(|v| v.as_array()) {
                for item in side_data {
                    if let Some(rot) = item.get("rotation") {
                        if let Some(r) = rot.as_i64() {
                            rotation = r as i32;
                        } else if let Some(r) = rot.as_f64() {
                            rotation = r as i32;
                        } else if let Some(s) = rot.as_str() {
                            rotation = s.parse().unwrap_or(0);
                        }
                    }
                }
            }
        }

        // FFmpeg autorotate swaps output dimensions for 90/270° rotation
        let rotation_abs = rotation.unsigned_abs() % 360;
        let (display_w, display_h) = if rotation_abs == 90 || rotation_abs == 270 {
            log::info!(
                "Video has {}° rotation, display dimensions {}x{} (coded {}x{})",
                rotation,
                coded_h,
                coded_w,
                coded_w,
                coded_h
            );
            (coded_h, coded_w)
        } else {
            (coded_w, coded_h)
        };

        if display_w > 0 && display_h > 0 {
            Some((display_w, display_h))
        } else {
            None
        }
    }

    fn extract_worker(
        ffmpeg_path: String,
        video_path: PathBuf,
        output_dir: PathBuf,
        total_frames: u32,
        frame_width: u32,
        frame_height: u32,
        tx: mpsc::Sender<ExtractionProgress>,
        cancel: Arc<AtomicBool>,
    ) {
        let lz4_dir = output_dir.join("lz4");
        let raw_dir = output_dir.join("raw");

        // Remove old completion marker (will be rewritten on success)
        let _ = std::fs::remove_file(output_dir.join(".complete"));

        // Clean up old lz4/raw subdirs and legacy flat files
        let _ = std::fs::remove_dir_all(&lz4_dir);
        let _ = std::fs::remove_dir_all(&raw_dir);
        if let Ok(entries) = std::fs::read_dir(&output_dir) {
            for entry in entries.flatten() {
                let ext = entry
                    .path()
                    .extension()
                    .map(|e| e.to_string_lossy().to_string());
                if matches!(ext.as_deref(), Some("png") | Some("lz4") | Some("raw")) {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }

        // Create fresh output directories
        if let Err(e) = std::fs::create_dir_all(&lz4_dir).and_then(|_| std::fs::create_dir_all(&raw_dir)) {
            let _ = tx.send(ExtractionProgress {
                total_frames,
                extracted_frames: 0,
                complete: true,
                failed: true,
                error_message: format!("Failed to create output dir: {e}"),
                warning_message: String::new(),
            });
            return;
        }

        // Probe actual video dimensions (codec resolution may differ from metadata)
        let (actual_width, actual_height) = match Self::probe_dimensions(&ffmpeg_path, &video_path)
        {
            Some((w, h)) => {
                if w != frame_width || h != frame_height {
                    log::info!(
                        "Video coded resolution {}x{} differs from metadata {}x{}, using probed",
                        w,
                        h,
                        frame_width,
                        frame_height
                    );
                }
                (w, h)
            }
            None => {
                log::warn!(
                    "ffprobe failed, using metadata dimensions {}x{}",
                    frame_width,
                    frame_height
                );
                (frame_width, frame_height)
            }
        };

        let frame_size = actual_width as usize * actual_height as usize * 4;
        let size_arg = format!("{}x{}", actual_width, actual_height);

        // FFmpeg: decode video to raw RGBA, -s ensures exact output dimensions
        // (avoids codec padding rows that would misalign raw frame reads)
        let mut child = match Command::new(&ffmpeg_path)
            .args([
                "-i",
                &video_path.to_string_lossy(),
                "-vsync",
                "0",
                "-s",
                &size_arg,
                "-pix_fmt",
                "rgba",
                "-f",
                "rawvideo",
                "pipe:1",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                let _ = tx.send(ExtractionProgress {
                    total_frames,
                    extracted_frames: 0,
                    complete: true,
                    failed: true,
                    error_message: format!("Failed to start ffmpeg at '{}': {e}", ffmpeg_path),
                    warning_message: String::new(),
                });
                return;
            }
        };

        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        // Drain stderr in background to prevent FFmpeg from blocking
        let stderr_handle = std::thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut buf = [0u8; 4096];
            while reader.read(&mut buf).unwrap_or(0) > 0 {}
        });

        // --- Three-stage pipeline: Reader → Compressors → IO Writers ---
        let num_workers = std::thread::available_parallelism()
            .map(|n| (n.get() * 3 / 4).max(2))
            .unwrap_or(2)
            .min(12);

        log::info!(
            "Extraction: {} compression workers + 2 IO writers, frame {}x{} ({:.1} MB/frame)",
            num_workers,
            actual_width,
            actual_height,
            frame_size as f64 / (1024.0 * 1024.0)
        );

        // Stage 1→2: raw frames to compression workers
        let frame_queue_size = num_workers * 4;
        let (frame_tx, frame_rx) = mpsc::sync_channel::<(u32, Vec<u8>)>(frame_queue_size);
        let frame_rx = Arc::new(Mutex::new(frame_rx));

        // Stage 2→3: raw + compressed frames to IO writers
        let (io_tx, io_rx) = mpsc::sync_channel::<(u32, Vec<u8>, Vec<u8>)>(frame_queue_size);
        let io_rx = Arc::new(Mutex::new(io_rx));

        // Buffer recycling: IO writers return used raw buffers to reader
        let (buf_return_tx, buf_return_rx) = mpsc::channel::<Vec<u8>>();

        // Pre-allocate buffers so the reader doesn't stall on zeroed allocation
        for _ in 0..(num_workers * 2) {
            let _ = buf_return_tx.send(vec![0u8; frame_size]);
        }

        // Shared state
        let frames_written = Arc::new(AtomicU32::new(0));
        let has_error = Arc::new(AtomicBool::new(false));
        let write_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let total_raw_bytes = Arc::new(AtomicU64::new(0));
        let total_compressed_bytes = Arc::new(AtomicU64::new(0));

        // Spawn IO writer threads (write both lz4 + raw, return raw buffer for reuse)
        let mut io_writers = Vec::with_capacity(2);
        for _ in 0..2 {
            let rx = io_rx.clone();
            let counter = frames_written.clone();
            let err_flag = has_error.clone();
            let err = write_error.clone();
            let l_dir = lz4_dir.clone();
            let r_dir = raw_dir.clone();
            let buf_tx = buf_return_tx.clone();
            io_writers.push(std::thread::spawn(move || {
                loop {
                    let (frame_num, raw, compressed) = match rx.lock().unwrap().recv() {
                        Ok(item) => item,
                        Err(_) => break,
                    };
                    let lz4_path = l_dir.join(format!("frame_{:06}.lz4", frame_num));
                    if let Err(e) = std::fs::write(&lz4_path, &compressed) {
                        let mut guard = err.lock().unwrap();
                        if guard.is_none() {
                            *guard = Some(format!("Failed to write frame {}: {e}", frame_num));
                        }
                        err_flag.store(true, Ordering::Relaxed);
                    }
                    let raw_path = r_dir.join(format!("frame_{:06}.raw", frame_num));
                    if let Err(e) = std::fs::write(&raw_path, &raw) {
                        let mut guard = err.lock().unwrap();
                        if guard.is_none() {
                            *guard = Some(format!("Failed to write frame {}: {e}", frame_num));
                        }
                        err_flag.store(true, Ordering::Relaxed);
                    }
                    // Return raw buffer for reuse by reader
                    let _ = buf_tx.send(raw);
                    counter.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }

        // Spawn compression worker threads (compress then hand off raw+compressed to IO)
        let mut comp_workers = Vec::with_capacity(num_workers);
        for _ in 0..num_workers {
            let rx = frame_rx.clone();
            let io = io_tx.clone();
            let raw_bytes = total_raw_bytes.clone();
            let comp_bytes = total_compressed_bytes.clone();
            comp_workers.push(std::thread::spawn(move || {
                loop {
                    let (frame_num, data) = match rx.lock().unwrap().recv() {
                        Ok(item) => item,
                        Err(_) => break, // Channel closed
                    };
                    let raw_len = data.len() as u64;
                    let compressed = lz4_flex::compress_prepend_size(&data);
                    let comp_len = compressed.len() as u64;

                    raw_bytes.fetch_add(raw_len, Ordering::Relaxed);
                    comp_bytes.fetch_add(comp_len, Ordering::Relaxed);

                    // Hand raw + compressed data to IO writers (buffer returned by IO)
                    if io.send((frame_num, data, compressed)).is_err() {
                        break;
                    }
                }
            }));
        }
        // Drop our copies so channels close only when workers finish
        drop(buf_return_tx);
        drop(io_tx);

        // Reader loop: read raw frames from FFmpeg stdout, send to compressors
        let mut reader = BufReader::with_capacity(frame_size.min(8 * 1024 * 1024), stdout);
        let mut frames_read = 0u32;

        loop {
            if cancel.load(Ordering::Relaxed) {
                let _ = child.kill();
                break;
            }
            if has_error.load(Ordering::Relaxed) {
                let _ = child.kill();
                break;
            }

            // Reuse a returned buffer or allocate a new one
            let mut frame_buf = buf_return_rx
                .try_recv()
                .unwrap_or_else(|_| vec![0u8; frame_size]);

            match reader.read_exact(&mut frame_buf) {
                Ok(()) => {
                    frames_read += 1;
                    if frame_tx.send((frames_read, frame_buf)).is_err() {
                        break;
                    }
                    // Report progress based on frames actually written to disk
                    let written = frames_written.load(Ordering::Relaxed);
                    let _ = tx.send(ExtractionProgress {
                        total_frames,
                        extracted_frames: written,
                        complete: false,
                        failed: false,
                        error_message: String::new(),
                        warning_message: String::new(),
                    });
                }
                Err(_) => break, // EOF or error - FFmpeg finished
            }
        }

        // Shutdown pipeline in order: reader → compressors → IO writers
        drop(frame_tx);
        for w in comp_workers {
            let _ = w.join();
        }
        // Compressor io_tx clones are now dropped → IO channel closes
        for w in io_writers {
            let _ = w.join();
        }

        let status = child.wait();
        let _ = stderr_handle.join();
        let success = status.map(|s| s.success()).unwrap_or(false);

        // Check for write errors from workers
        let worker_error = write_error.lock().unwrap().take();

        // Count actual LZ4 files produced
        let actual_count = std::fs::read_dir(&lz4_dir)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .filter(|e| e.path().extension().is_some_and(|ext| ext == "lz4"))
                    .count() as u32
            })
            .unwrap_or(0);

        let (failed, error_message, warning_message) = if cancel.load(Ordering::Relaxed) {
            (true, "Extraction cancelled".into(), String::new())
        } else if let Some(err) = worker_error {
            (true, err, String::new())
        } else if !success && actual_count == 0 {
            (true, "ffmpeg exited with error".into(), String::new())
        } else if actual_count == 0 {
            (true, "No frames were extracted".into(), String::new())
        } else if actual_count != total_frames {
            (
                false,
                String::new(),
                format!(
                    "Frame count mismatch: MP4 produced {} frames but JSON expects {}",
                    actual_count, total_frames
                ),
            )
        } else {
            (false, String::new(), String::new())
        };

        // Log compression ratio
        let raw = total_raw_bytes.load(Ordering::Relaxed);
        let compressed = total_compressed_bytes.load(Ordering::Relaxed);
        if raw > 0 && compressed > 0 {
            log::info!(
                "LZ4 compression: {} frames, raw {:.1} MB -> compressed {:.1} MB ({:.2}x ratio, {:.1}% savings)",
                actual_count,
                raw as f64 / (1024.0 * 1024.0),
                compressed as f64 / (1024.0 * 1024.0),
                raw as f64 / compressed as f64,
                (1.0 - compressed as f64 / raw as f64) * 100.0,
            );
        }

        // Write completion marker (v3 format with actual dimensions)
        if !failed {
            let marker = output_dir.join(".complete");
            let _ = std::fs::write(
                &marker,
                format!("v3:{}:{}:{}", actual_count, actual_width, actual_height),
            );
        }

        let _ = tx.send(ExtractionProgress {
            total_frames,
            extracted_frames: actual_count,
            complete: true,
            failed,
            error_message,
            warning_message,
        });
    }
}

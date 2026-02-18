pub mod windows;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::mpsc;
use std::time::Instant;

/// Result of running a subprocess.
pub struct SubprocessResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl From<Output> for SubprocessResult {
    fn from(output: Output) -> Self {
        Self {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }
}

/// Get the directory containing the current executable.
pub fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Run a subprocess and capture its output.
pub fn run_process(executable: &str, args: &[&str]) -> Result<SubprocessResult, String> {
    std::process::Command::new(executable)
        .args(args)
        .output()
        .map(SubprocessResult::from)
        .map_err(|e| format!("Failed to run {executable}: {e}"))
}

// --- Performance monitoring ---

/// Process performance statistics.
#[derive(Debug, Clone, Default)]
pub struct ProcessPerf {
    pub fps: f32,
    pub cpu_percent: f32,
    /// Process working set (physical RAM) in MB.
    pub memory_mb: f32,
    /// Our working set + system available memory (effective pool) in MB.
    pub total_memory_mb: f32,
    pub gpu_percent: Option<f32>,
    pub gpu_memory_mb: Option<f32>,
    pub gpu_total_mb: Option<f32>,
    /// Videos folder: (folder_size_bytes, free_space_bytes_on_drive)
    pub video_disk: Option<(u64, u64)>,
    /// Cache folder disk usage: (used_mb, limit_mb)
    /// used_mb = actual cache dir size, limit_mb = settings limit or disk total
    pub cache_disk: Option<(f32, f32)>,
}

/// Tracks process performance metrics (FPS, CPU, Memory).
pub struct PerfMonitor {
    sys: sysinfo::System,
    pid: sysinfo::Pid,
    num_cpus: f32,
    last_sys_refresh: Instant,
    frame_times: VecDeque<Instant>,
    perf: ProcessPerf,
    // Paths for disk monitoring (set once)
    video_dir: Option<PathBuf>,
    cache_dir: Option<PathBuf>,
    cache_limit_mb: u32,
    gpu_cache_mb: u32,
    // Async disk size computation (avoid blocking UI thread)
    pending_disk_query: Option<mpsc::Receiver<(u64, u64)>>,
    last_disk_refresh: Instant,
    cached_video_dir_size: u64,
    cached_cache_dir_size: u64,
}

impl PerfMonitor {
    pub fn new() -> Self {
        let pid = sysinfo::Pid::from_u32(std::process::id());
        let mut sys = sysinfo::System::new();

        sys.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::Some(&[pid]),
            true,
            sysinfo::ProcessRefreshKind::new()
                .with_cpu()
                .with_memory(),
        );
        sys.refresh_memory();

        let num_cpus = std::thread::available_parallelism()
            .map(|n| n.get() as f32)
            .unwrap_or(1.0);
        let perf = ProcessPerf::default();

        Self {
            sys,
            pid,
            num_cpus,
            last_sys_refresh: Instant::now(),
            frame_times: VecDeque::with_capacity(120),
            perf,
            video_dir: None,
            cache_dir: None,
            cache_limit_mb: 10240,
            gpu_cache_mb: 2048,
            pending_disk_query: None,
            last_disk_refresh: Instant::now(),
            cached_video_dir_size: 0,
            cached_cache_dir_size: 0,
        }
    }

    /// Set the paths used for disk monitoring and GPU cache limit.
    pub fn set_disk_paths(&mut self, video_dir: &Path, cache_dir: &Path, cache_limit_mb: u32, gpu_cache_mb: u32) {
        self.video_dir = Some(video_dir.to_path_buf());
        self.cache_dir = Some(cache_dir.to_path_buf());
        self.cache_limit_mb = cache_limit_mb;
        self.gpu_cache_mb = gpu_cache_mb;
    }

    /// Call once per frame to update FPS and (periodically) system stats.
    pub fn tick(&mut self) {
        let now = Instant::now();

        // FPS: keep last 60 frame timestamps
        self.frame_times.push_back(now);
        while self.frame_times.len() > 60 {
            self.frame_times.pop_front();
        }
        if self.frame_times.len() >= 2 {
            let elapsed = now.duration_since(*self.frame_times.front().unwrap());
            let secs = elapsed.as_secs_f32();
            if secs > 0.0 {
                self.perf.fps = (self.frame_times.len() - 1) as f32 / secs;
            }
        }

        // Poll completed background disk size computation (cheap, every tick)
        if let Some(rx) = &self.pending_disk_query {
            if let Ok((vsize, csize)) = rx.try_recv() {
                self.cached_video_dir_size = vsize;
                self.cached_cache_dir_size = csize;
                self.pending_disk_query = None;
            }
        }

        // Refresh system stats every ~1 second (expensive)
        if self.last_sys_refresh.elapsed().as_secs_f32() >= 1.0 {
            self.last_sys_refresh = now;

            self.sys.refresh_processes_specifics(
                sysinfo::ProcessesToUpdate::Some(&[self.pid]),
                true,
                sysinfo::ProcessRefreshKind::new()
                    .with_cpu()
                    .with_memory(),
            );
            self.sys.refresh_memory();

            if let Some(proc) = self.sys.process(self.pid) {
                self.perf.cpu_percent = proc.cpu_usage() / self.num_cpus;
                let mem_mb = proc.memory() as f32 / (1024.0 * 1024.0);
                self.perf.memory_mb = mem_mb;
                let avail_mb = self.sys.available_memory() as f32 / (1024.0 * 1024.0);
                self.perf.total_memory_mb = mem_mb + avail_mb;
            }

            // GPU: platform-specific
            self.perf.gpu_percent = windows::gpu_usage_percent();
            if let Some((used, budget)) = windows::gpu_memory_info() {
                self.perf.gpu_memory_mb = Some(used);
                self.perf.gpu_total_mb = Some((self.gpu_cache_mb as f32).min(budget));
            }

            // Launch background disk size computation every ~5 seconds
            // (get_dir_size is slow on large directories, must not block UI thread)
            if self.pending_disk_query.is_none()
                && self.last_disk_refresh.elapsed().as_secs_f32() >= 5.0
            {
                self.last_disk_refresh = now;
                if let (Some(vdir), Some(cdir)) = (&self.video_dir, &self.cache_dir) {
                    let vdir = vdir.clone();
                    let cdir = cdir.clone();
                    let (tx, rx) = mpsc::channel();
                    self.pending_disk_query = Some(rx);
                    std::thread::spawn(move || {
                        let vsize = crate::video::cache::get_dir_size(&vdir);
                        let csize = crate::video::cache::get_dir_size(&cdir);
                        let _ = tx.send((vsize, csize));
                    });
                }
            }

            // Disk: video folder size vs free space on drive (using cached dir sizes)
            if let Some(vdir) = &self.video_dir {
                if self.cached_video_dir_size > 0 {
                    if let Some((disk_used, disk_total)) = windows::disk_space(vdir) {
                        let free = disk_total - disk_used;
                        self.perf.video_disk = Some((self.cached_video_dir_size, free));
                    }
                }
            }

            // Disk: cache folder size vs limit (using cached dir sizes)
            if self.cache_dir.is_some() {
                let cache_used_mb = self.cached_cache_dir_size as f32 / (1024.0 * 1024.0);
                let disk_limit_mb = if let Some(cdir) = &self.cache_dir {
                    if let Some((_used, total)) = windows::disk_space(cdir) {
                        (self.cache_limit_mb as f32).min(total as f32 / (1024.0 * 1024.0))
                    } else {
                        self.cache_limit_mb as f32
                    }
                } else {
                    self.cache_limit_mb as f32
                };
                self.perf.cache_disk = Some((cache_used_mb, disk_limit_mb));
            }
        }
    }

    pub fn perf(&self) -> &ProcessPerf {
        &self.perf
    }
}

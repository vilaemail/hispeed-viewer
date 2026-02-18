use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Per-video settings stored alongside the video file as `filename.settings.json`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VideoSettings {
    #[serde(default)]
    pub time_offset_seconds: f64,
    #[serde(default = "default_overlay_x")]
    pub overlay_x: f32,
    #[serde(default = "default_overlay_y")]
    pub overlay_y: f32,
}

impl VideoSettings {
    pub fn settings_path(video_path: &Path) -> PathBuf {
        video_path.with_extension("settings.json")
    }

    pub fn load(video_path: &Path) -> Self {
        let path = Self::settings_path(video_path);
        match std::fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, video_path: &Path) -> Result<(), String> {
        let path = Self::settings_path(video_path);
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize: {e}"))?;
        std::fs::write(&path, json)
            .map_err(|e| format!("Failed to write {}: {e}", path.display()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceType {
    Http,
    Folder,
    Adb,
}

impl Default for SourceType {
    fn default() -> Self {
        Self::Folder
    }
}

impl SourceType {
    pub const ALL: &[SourceType] = &[Self::Http, Self::Folder, Self::Adb];

    pub fn label(&self) -> &'static str {
        match self {
            Self::Http => "HTTP Server",
            Self::Folder => "Local Folder",
            Self::Adb => "ADB",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ViewerMode {
    SingleFrame,
    Filmstrip,
}

impl Default for ViewerMode {
    fn default() -> Self {
        Self::SingleFrame
    }
}

impl ViewerMode {
    pub const ALL: &[ViewerMode] = &[Self::SingleFrame, Self::Filmstrip];

    pub fn label(&self) -> &'static str {
        match self {
            Self::SingleFrame => "Single Frame",
            Self::Filmstrip => "Filmstrip",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimeFormat {
    Milliseconds,
    Microseconds,
}

impl Default for TimeFormat {
    fn default() -> Self {
        Self::Milliseconds
    }
}

impl TimeFormat {
    pub const ALL: &[TimeFormat] = &[Self::Milliseconds, Self::Microseconds];

    pub fn label(&self) -> &'static str {
        match self {
            Self::Milliseconds => "Milliseconds",
            Self::Microseconds => "Microseconds",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "default_http_url")]
    pub http_url: String,
    #[serde(default = "default_folder_path")]
    pub folder_path: String,
    #[serde(default = "default_adb_device_path")]
    pub adb_device_path: String,
    #[serde(default = "default_adb_path")]
    pub adb_path: String,
    #[serde(default = "default_ffmpeg_path")]
    pub ffmpeg_path: String,
    #[serde(default = "default_temp_folder")]
    pub temp_folder: String,
    #[serde(default = "default_completed_folder")]
    pub completed_folder: String,
    #[serde(default = "default_cache_folder")]
    pub cache_folder: String,
    #[serde(default)]
    pub active_source: SourceType,
    #[serde(default)]
    pub viewer_mode: ViewerMode,
    #[serde(default = "default_overlay_x")]
    pub time_overlay_x: f32,
    #[serde(default = "default_overlay_y")]
    pub time_overlay_y: f32,
    #[serde(default = "default_disk_cache_mb")]
    pub disk_cache_mb: u32,
    #[serde(default = "default_gpu_cache_mb")]
    pub gpu_cache_mb: u32,
    #[serde(default)]
    pub time_format: TimeFormat,
    #[serde(default = "default_filmstrip_before_count")]
    pub filmstrip_before_count: u32,
    #[serde(default = "default_filmstrip_after_count")]
    pub filmstrip_after_count: u32,
    #[serde(default = "default_filmstrip_center_percent")]
    pub filmstrip_center_percent: u32,
}

fn default_http_url() -> String {
    "http://192.168.1.100:8080".into()
}
fn default_folder_path() -> String {
    "E:\\hispeed-trigger-cam".into()
}
fn default_adb_device_path() -> String {
    "/storage/emulated/0/DCIM/hispeed-trigger-cam".into()
}
fn default_adb_path() -> String {
    "adb".into()
}
fn default_ffmpeg_path() -> String {
    "ffmpeg".into()
}
fn default_temp_folder() -> String {
    "import-temporary".into()
}
fn default_completed_folder() -> String {
    "videos".into()
}
fn default_cache_folder() -> String {
    "cache".into()
}
fn default_overlay_x() -> f32 {
    10.0
}
fn default_overlay_y() -> f32 {
    10.0
}
fn default_filmstrip_before_count() -> u32 {
    4
}
fn default_filmstrip_after_count() -> u32 {
    4
}
fn default_filmstrip_center_percent() -> u32 {
    150
}
fn default_disk_cache_mb() -> u32 {
    20480
}
fn default_gpu_cache_mb() -> u32 {
    8192
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            http_url: default_http_url(),
            folder_path: default_folder_path(),
            adb_device_path: default_adb_device_path(),
            adb_path: default_adb_path(),
            ffmpeg_path: default_ffmpeg_path(),
            temp_folder: default_temp_folder(),
            completed_folder: default_completed_folder(),
            cache_folder: default_cache_folder(),
            active_source: SourceType::default(),
            viewer_mode: ViewerMode::default(),
            time_overlay_x: default_overlay_x(),
            time_overlay_y: default_overlay_y(),
            disk_cache_mb: default_disk_cache_mb(),
            gpu_cache_mb: default_gpu_cache_mb(),
            time_format: TimeFormat::default(),
            filmstrip_before_count: default_filmstrip_before_count(),
            filmstrip_after_count: default_filmstrip_after_count(),
            filmstrip_center_percent: default_filmstrip_center_percent(),
        }
    }
}

impl Settings {
    /// Load settings from a JSON file, returning defaults if the file doesn't exist or is invalid.
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_else(|e| {
                log::warn!("Failed to parse settings: {e}, using defaults");
                Self::default()
            }),
            Err(_) => {
                log::info!("No settings file found at {}, using defaults", path.display());
                Self::default()
            }
        }
    }

    /// Save settings to a JSON file.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize settings: {e}"))?;
        std::fs::write(path, json)
            .map_err(|e| format!("Failed to write settings to {}: {e}", path.display()))
    }

    /// Resolve a potentially relative path against the executable directory.
    pub fn resolve_path(&self, exe_dir: &Path, path: &str) -> PathBuf {
        let p = Path::new(path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            exe_dir.join(p)
        }
    }

    /// Resolve all folder paths to absolute for UI display.
    pub fn resolve_all_paths(&mut self, exe_dir: &Path) {
        self.folder_path = self.resolve_path(exe_dir, &self.folder_path).to_string_lossy().into_owned();
        self.temp_folder = self.resolve_path(exe_dir, &self.temp_folder).to_string_lossy().into_owned();
        self.completed_folder = self.resolve_path(exe_dir, &self.completed_folder).to_string_lossy().into_owned();
        self.cache_folder = self.resolve_path(exe_dir, &self.cache_folder).to_string_lossy().into_owned();
    }

    /// Convert absolute paths back to relative (within exe_dir) for storage.
    pub fn relativize_all_paths(&mut self, exe_dir: &Path) {
        self.folder_path = Self::to_relative(exe_dir, &self.folder_path);
        self.adb_path = Self::to_relative(exe_dir, &self.adb_path);
        self.ffmpeg_path = Self::to_relative(exe_dir, &self.ffmpeg_path);
        self.temp_folder = Self::to_relative(exe_dir, &self.temp_folder);
        self.completed_folder = Self::to_relative(exe_dir, &self.completed_folder);
        self.cache_folder = Self::to_relative(exe_dir, &self.cache_folder);
    }

    fn to_relative(exe_dir: &Path, path: &str) -> String {
        let p = Path::new(path);
        if let Ok(rel) = p.strip_prefix(exe_dir) {
            let s = rel.to_string_lossy().into_owned();
            if s.is_empty() { ".".into() } else { s }
        } else {
            path.to_string()
        }
    }

    /// Compute how many frames fit in the GPU cache based on frame dimensions.
    pub fn gpu_cache_frame_count(&self, width: u32, height: u32) -> usize {
        let frame_bytes = width as u64 * height as u64 * 4; // RGBA
        if frame_bytes == 0 {
            return 30; // fallback
        }
        let max = (self.gpu_cache_mb as u64 * 1024 * 1024) / frame_bytes;
        max.max(5) as usize
    }

    pub fn temp_dir(&self, exe_dir: &Path) -> PathBuf {
        self.resolve_path(exe_dir, &self.temp_folder)
    }

    pub fn completed_dir(&self, exe_dir: &Path) -> PathBuf {
        self.resolve_path(exe_dir, &self.completed_folder)
    }

    pub fn cache_dir(&self, exe_dir: &Path) -> PathBuf {
        self.resolve_path(exe_dir, &self.cache_folder)
    }

    /// Validate settings configuration.
    /// Checks that tools (ffmpeg, adb) can be executed and that storage folders
    /// exist or can be created. Does NOT check source availability.
    /// Returns a list of error messages (empty = all OK).
    pub fn validate(&self, exe_dir: &Path) -> Vec<String> {
        let mut errors = Vec::new();

        // Check ffmpeg executable
        match std::process::Command::new(&self.ffmpeg_path)
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
        {
            Ok(status) if status.success() => {}
            Ok(_) => errors.push(format!(
                "FFmpeg at '{}' exited with error",
                self.ffmpeg_path
            )),
            Err(_) => errors.push(format!(
                "FFmpeg not found or cannot run at '{}'",
                self.ffmpeg_path
            )),
        }

        // Check adb executable
        match std::process::Command::new(&self.adb_path)
            .arg("version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
        {
            Ok(status) if status.success() => {}
            Ok(_) => errors.push(format!(
                "ADB at '{}' exited with error",
                self.adb_path
            )),
            Err(_) => errors.push(format!(
                "ADB not found or cannot run at '{}'",
                self.adb_path
            )),
        }

        // Check/create storage folders
        let folders = [
            ("Videos folder", self.completed_dir(exe_dir)),
            ("Import temp folder", self.temp_dir(exe_dir)),
            ("Frame cache folder", self.cache_dir(exe_dir)),
        ];
        for (label, path) in &folders {
            if !path.exists() {
                if let Err(e) = std::fs::create_dir_all(path) {
                    errors.push(format!(
                        "{} cannot be created at '{}': {}",
                        label,
                        path.display(),
                        e
                    ));
                }
            }
        }

        errors
    }
}

use crate::source::traits::*;
use crate::sync::integrity;
use crate::video::metadata::VideoMetadata;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A file that has been fully synced and verified.
#[derive(Debug, Clone)]
pub struct SyncedFile {
    pub json_name: String,
    pub metadata: VideoMetadata,
    pub video_path: PathBuf,
    pub json_path: PathBuf,
}

/// A modification detected on the remote that needs user approval.
#[derive(Debug, Clone)]
pub struct PendingChange {
    pub json_name: String,
    pub description: String,
    pub old_size: u64,
    pub new_size: u64,
}

/// Messages sent from the sync thread to the main thread.
#[derive(Debug)]
pub enum SyncMessage {
    /// Refreshed list of completed files.
    CompletedFiles(Vec<SyncedFile>),
    /// New pending changes detected.
    PendingChanges(Vec<PendingChange>),
    /// Status update text.
    Status(String),
    /// Source availability changed.
    SourceStatus(SourceStatus),
}

/// Orchestrates file synchronization between a remote source and local storage.
pub struct FileSync {
    tx: mpsc::Sender<SyncCommand>,
    rx: mpsc::Receiver<SyncMessage>,
    _running: Arc<AtomicBool>,
    _handle: Option<std::thread::JoinHandle<()>>,

    // Cached state read by the UI
    pub completed_files: Vec<SyncedFile>,
    pub pending_changes: Vec<PendingChange>,
    pub source_status: SourceStatus,
    pub status_text: String,
}

enum SyncCommand {
    Sync,
    ApproveChange(String),
    Stop,
}

impl FileSync {
    pub fn new(
        mut source: Box<dyn RemoteSource>,
        temp_dir: PathBuf,
        completed_dir: PathBuf,
    ) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<SyncCommand>();
        let (msg_tx, msg_rx) = mpsc::channel::<SyncMessage>();
        let running = Arc::new(AtomicBool::new(true));
        let running_clone = running.clone();

        // Ensure directories exist
        let _ = std::fs::create_dir_all(&temp_dir);
        let _ = std::fs::create_dir_all(&completed_dir);

        let handle = std::thread::Builder::new()
            .name("file-sync".into())
            .spawn(move || {
                Self::sync_loop(
                    &mut *source,
                    &temp_dir,
                    &completed_dir,
                    cmd_rx,
                    msg_tx,
                    running_clone,
                );
            })
            .expect("Failed to spawn sync thread");

        Self {
            tx: cmd_tx,
            rx: msg_rx,
            _running: running,
            _handle: Some(handle),
            completed_files: Vec::new(),
            pending_changes: Vec::new(),
            source_status: SourceStatus::Checking,
            status_text: "Starting...".into(),
        }
    }

    /// Trigger a sync now (non-blocking).
    pub fn trigger_sync(&self) {
        let _ = self.tx.send(SyncCommand::Sync);
    }

    /// Approve a pending change.
    pub fn approve_change(&self, json_name: &str) {
        let _ = self.tx.send(SyncCommand::ApproveChange(json_name.to_string()));
    }

    /// Poll for updates from the sync thread. Call each frame.
    pub fn poll(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                SyncMessage::CompletedFiles(files) => self.completed_files = files,
                SyncMessage::PendingChanges(changes) => self.pending_changes = changes,
                SyncMessage::Status(text) => self.status_text = text,
                SyncMessage::SourceStatus(status) => self.source_status = status,
            }
        }
    }

    fn sync_loop(
        source: &mut dyn RemoteSource,
        temp_dir: &Path,
        completed_dir: &Path,
        cmd_rx: mpsc::Receiver<SyncCommand>,
        msg_tx: mpsc::Sender<SyncMessage>,
        running: Arc<AtomicBool>,
    ) {
        // Initial scan of completed directory
        let completed = Self::scan_completed(completed_dir);
        let _ = msg_tx.send(SyncMessage::CompletedFiles(completed));

        // First auto-sync starts immediately
        let mut next_auto_sync = std::time::Instant::now();

        while running.load(Ordering::Relaxed) {
            // Check for commands
            match cmd_rx.try_recv() {
                Ok(SyncCommand::Stop) => break,
                Ok(SyncCommand::Sync) => {
                    let success = Self::perform_sync(source, temp_dir, completed_dir, &msg_tx);
                    next_auto_sync = Self::next_sync_time(success);
                }
                Ok(SyncCommand::ApproveChange(json_name)) => {
                    Self::apply_change(source, temp_dir, completed_dir, &json_name, &msg_tx);
                }
                Err(mpsc::TryRecvError::Disconnected) => break,
                Err(mpsc::TryRecvError::Empty) => {}
            }

            // Auto sync when timer expires
            if std::time::Instant::now() >= next_auto_sync {
                let success = Self::perform_sync(source, temp_dir, completed_dir, &msg_tx);
                next_auto_sync = Self::next_sync_time(success);
            }

            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    fn next_sync_time(success: bool) -> std::time::Instant {
        let secs = if success { 30 } else { 5 };
        std::time::Instant::now() + std::time::Duration::from_secs(secs)
    }

    /// Returns true if sync completed successfully, false on failure.
    fn perform_sync(
        source: &mut dyn RemoteSource,
        temp_dir: &Path,
        completed_dir: &Path,
        msg_tx: &mpsc::Sender<SyncMessage>,
    ) -> bool {
        let _ = msg_tx.send(SyncMessage::Status("Checking source...".into()));

        source.refresh_status();
        let _ = msg_tx.send(SyncMessage::SourceStatus(source.status()));

        if source.status() != SourceStatus::Available {
            let _ = msg_tx.send(SyncMessage::Status("Source unavailable".into()));
            return false;
        }

        let _ = msg_tx.send(SyncMessage::Status("Listing remote files...".into()));
        let remote_files = source.list_files();

        if remote_files.is_empty() {
            let _ = msg_tx.send(SyncMessage::Status("No files on remote".into()));
            return true;
        }

        let mut pending_changes = Vec::new();
        let mut did_change = false;

        for remote in &remote_files {
            let local_json = completed_dir.join(&remote.json_name);
            let stem = remote.json_name.trim_end_matches(".json");
            let local_video = completed_dir.join(format!("{stem}.mp4"));

            if local_json.exists() && local_video.exists() {
                // Check for modifications: re-fetch JSON and compare
                let temp_json = temp_dir.join(&remote.json_name);
                let result = source.fetch_file(&remote.json_name, &temp_json);
                if result.success {
                    let old_content = std::fs::read_to_string(&local_json).unwrap_or_default();
                    let new_content = std::fs::read_to_string(&temp_json).unwrap_or_default();

                    if old_content != new_content {
                        // Parse both to get size info
                        let old_meta = VideoMetadata::from_str(&old_content);
                        let new_meta = VideoMetadata::from_str(&new_content);

                        pending_changes.push(PendingChange {
                            json_name: remote.json_name.clone(),
                            description: describe_changes(&old_meta, &new_meta),
                            old_size: old_meta.as_ref().map(|m| m.size).unwrap_or(0),
                            new_size: new_meta.as_ref().map(|m| m.size).unwrap_or(0),
                        });
                    }
                    let _ = std::fs::remove_file(&temp_json);
                }
            } else if !local_json.exists() || !local_video.exists() {
                // New file - fetch it
                let _ = msg_tx.send(SyncMessage::Status(format!(
                    "Fetching {}...",
                    remote.json_name
                )));

                if Self::fetch_and_verify(source, temp_dir, completed_dir, &remote.json_name, msg_tx) {
                    did_change = true;
                }
            }
        }

        if !pending_changes.is_empty() {
            let _ = msg_tx.send(SyncMessage::PendingChanges(pending_changes));
        }

        if did_change {
            let completed = Self::scan_completed(completed_dir);
            let _ = msg_tx.send(SyncMessage::CompletedFiles(completed));
        }

        let _ = msg_tx.send(SyncMessage::Status("Idle".into()));
        true
    }

    fn fetch_and_verify(
        source: &mut dyn RemoteSource,
        temp_dir: &Path,
        completed_dir: &Path,
        json_name: &str,
        msg_tx: &mpsc::Sender<SyncMessage>,
    ) -> bool {
        let stem = json_name.trim_end_matches(".json");
        let video_name = format!("{stem}.mp4");

        let temp_json = temp_dir.join(json_name);
        let temp_video = temp_dir.join(&video_name);

        // Fetch JSON
        let result = source.fetch_file(json_name, &temp_json);
        if !result.success {
            log::warn!("Failed to fetch {json_name}: {:?}", result.error);
            return false;
        }

        // Parse metadata
        let metadata = match VideoMetadata::from_file(&temp_json) {
            Ok(m) => m,
            Err(e) => {
                log::warn!("Failed to parse {json_name}: {e}");
                let _ = std::fs::remove_file(&temp_json);
                return false;
            }
        };

        // Fetch video
        let _ = msg_tx.send(SyncMessage::Status(format!("Downloading {video_name}...")));
        let result = source.fetch_file(&video_name, &temp_video);
        if !result.success {
            log::warn!("Failed to fetch {video_name}: {:?}", result.error);
            let _ = std::fs::remove_file(&temp_json);
            return false;
        }

        // Verify SHA-256
        let _ = msg_tx.send(SyncMessage::Status(format!("Verifying {video_name}...")));
        match integrity::verify_sha256(&temp_video, &metadata.sha256) {
            Ok(true) => {
                // Move to completed
                let dest_json = completed_dir.join(json_name);
                let dest_video = completed_dir.join(&video_name);

                if std::fs::rename(&temp_json, &dest_json).is_err() {
                    let _ = std::fs::copy(&temp_json, &dest_json);
                    let _ = std::fs::remove_file(&temp_json);
                }
                if std::fs::rename(&temp_video, &dest_video).is_err() {
                    let _ = std::fs::copy(&temp_video, &dest_video);
                    let _ = std::fs::remove_file(&temp_video);
                }

                log::info!("Synced {json_name} successfully");
                true
            }
            Ok(false) => {
                log::warn!("SHA-256 mismatch for {video_name}");
                let _ = std::fs::remove_file(&temp_json);
                let _ = std::fs::remove_file(&temp_video);
                false
            }
            Err(e) => {
                log::warn!("Hash verification failed for {video_name}: {e}");
                let _ = std::fs::remove_file(&temp_json);
                let _ = std::fs::remove_file(&temp_video);
                false
            }
        }
    }

    fn apply_change(
        source: &mut dyn RemoteSource,
        temp_dir: &Path,
        completed_dir: &Path,
        json_name: &str,
        msg_tx: &mpsc::Sender<SyncMessage>,
    ) {
        let _ = msg_tx.send(SyncMessage::Status(format!("Applying update for {json_name}...")));
        let success = Self::fetch_and_verify(source, temp_dir, completed_dir, json_name, msg_tx);
        if success {
            let completed = Self::scan_completed(completed_dir);
            let _ = msg_tx.send(SyncMessage::CompletedFiles(completed));
        }
        let _ = msg_tx.send(SyncMessage::Status("Idle".into()));
    }

    fn scan_completed(completed_dir: &Path) -> Vec<SyncedFile> {
        let mut files = Vec::new();

        let entries = match std::fs::read_dir(completed_dir) {
            Ok(e) => e,
            Err(_) => return files,
        };

        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "json") {
                let video_path = path.with_extension("mp4");
                if video_path.exists() {
                    if let Ok(metadata) = VideoMetadata::from_file(&path) {
                        files.push(SyncedFile {
                            json_name: path.file_name().unwrap_or_default().to_string_lossy().into_owned(),
                            metadata,
                            video_path,
                            json_path: path,
                        });
                    }
                }
            }
        }

        files.sort_by(|a, b| b.json_name.cmp(&a.json_name));
        files
    }
}

impl Drop for FileSync {
    fn drop(&mut self) {
        let _ = self.tx.send(SyncCommand::Stop);
        self._running.store(false, Ordering::Relaxed);
    }
}

fn describe_changes(
    old: &Result<VideoMetadata, String>,
    new: &Result<VideoMetadata, String>,
) -> String {
    match (old, new) {
        (Ok(old), Ok(new)) => {
            let mut changes = Vec::new();
            if old.size != new.size {
                changes.push(format!(
                    "Size: {} -> {}",
                    old.size, new.size
                ));
            }
            if old.total_frames != new.total_frames {
                changes.push(format!(
                    "Frames: {} -> {}",
                    old.total_frames, new.total_frames
                ));
            }
            if old.sha256 != new.sha256 {
                changes.push("Hash changed".into());
            }
            if changes.is_empty() {
                "Metadata changed".into()
            } else {
                changes.join(", ")
            }
        }
        _ => "Metadata changed".into(),
    }
}

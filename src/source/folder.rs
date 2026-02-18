use super::traits::*;
use std::path::{Path, PathBuf};

pub struct FolderSource {
    folder_path: PathBuf,
    status: SourceStatus,
}

impl FolderSource {
    pub fn new(folder_path: &str) -> Self {
        Self {
            folder_path: PathBuf::from(folder_path),
            status: SourceStatus::Checking,
        }
    }

    pub fn set_path(&mut self, path: &str) {
        self.folder_path = PathBuf::from(path);
        self.status = SourceStatus::Checking;
    }
}

impl RemoteSource for FolderSource {
    fn status(&self) -> SourceStatus {
        self.status
    }

    fn refresh_status(&mut self) {
        self.status = if self.folder_path.is_dir() {
            // Try to actually read the directory to verify access
            match std::fs::read_dir(&self.folder_path) {
                Ok(_) => SourceStatus::Available,
                Err(_) => SourceStatus::Unavailable,
            }
        } else {
            SourceStatus::Unavailable
        };
    }

    fn list_files(&mut self) -> Vec<RemoteFileEntry> {
        if self.status != SourceStatus::Available {
            return Vec::new();
        }

        let entries = match std::fs::read_dir(&self.folder_path) {
            Ok(e) => e,
            Err(e) => {
                log::warn!("Failed to read folder {}: {e}", self.folder_path.display());
                self.status = SourceStatus::Unavailable;
                return Vec::new();
            }
        };

        entries
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .is_some_and(|ext| ext == "json")
            })
            .map(|e| {
                let size = e.metadata().map(|m| m.len() as i64).unwrap_or(0);
                RemoteFileEntry {
                    json_name: e.file_name().to_string_lossy().into_owned(),
                    size,
                }
            })
            .collect()
    }

    fn fetch_file(&self, remote_name: &str, dest: &Path) -> FetchResult {
        let src = self.folder_path.join(remote_name);
        match std::fs::copy(&src, dest) {
            Ok(_) => FetchResult::ok(),
            Err(e) => FetchResult::err(format!(
                "Failed to copy {} -> {}: {e}",
                src.display(),
                dest.display()
            )),
        }
    }

    fn display_name(&self) -> &str {
        "Local Folder"
    }
}

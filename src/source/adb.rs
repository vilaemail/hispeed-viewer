use super::traits::*;
use crate::platform;
use std::path::Path;

pub struct AdbSource {
    adb_path: String,
    device_path: String,
    status: SourceStatus,
}

impl AdbSource {
    pub fn new(adb_path: &str, device_path: &str) -> Self {
        Self {
            adb_path: adb_path.to_string(),
            device_path: device_path.trim_end_matches('/').to_string(),
            status: SourceStatus::Checking,
        }
    }
}

impl RemoteSource for AdbSource {
    fn status(&self) -> SourceStatus {
        self.status
    }

    fn refresh_status(&mut self) {
        match platform::run_process(&self.adb_path, &["devices"]) {
            Ok(result) if result.exit_code == 0 => {
                // Check if any device is connected (more than header line)
                let lines: Vec<&str> = result.stdout.lines().collect();
                let has_device = lines.iter().any(|l| l.contains("device") && !l.contains("List"));
                self.status = if has_device {
                    SourceStatus::Available
                } else {
                    SourceStatus::Unavailable
                };
            }
            _ => {
                self.status = SourceStatus::Unavailable;
            }
        }
    }

    fn list_files(&mut self) -> Vec<RemoteFileEntry> {
        if self.status != SourceStatus::Available {
            return Vec::new();
        }

        let ls_cmd = format!("ls -la {}", self.device_path);
        match platform::run_process(&self.adb_path, &["shell", &ls_cmd]) {
            Ok(result) if result.exit_code == 0 => {
                result
                    .stdout
                    .lines()
                    .filter_map(|line| {
                        // Parse `ls -la` output: permissions links owner group size date name
                        let parts: Vec<&str> = line.split_whitespace().collect();
                        if parts.len() >= 7 {
                            let name = parts.last()?.to_string();
                            if name.ends_with(".json") {
                                let size = parts.get(4).and_then(|s| s.parse().ok()).unwrap_or(0);
                                return Some(RemoteFileEntry {
                                    json_name: name,
                                    size,
                                });
                            }
                        }
                        None
                    })
                    .collect()
            }
            Ok(result) => {
                log::warn!("adb ls failed: {}", result.stderr);
                self.status = SourceStatus::Unavailable;
                Vec::new()
            }
            Err(e) => {
                log::warn!("adb error: {e}");
                self.status = SourceStatus::Unavailable;
                Vec::new()
            }
        }
    }

    fn fetch_file(&self, remote_name: &str, dest: &Path) -> FetchResult {
        let remote_path = format!("{}/{}", self.device_path, remote_name);
        let dest_str = dest.to_string_lossy();

        match platform::run_process(&self.adb_path, &["pull", &remote_path, &dest_str]) {
            Ok(result) if result.exit_code == 0 => FetchResult::ok(),
            Ok(result) => FetchResult::err(format!("adb pull failed: {}", result.stderr)),
            Err(e) => FetchResult::err(format!("adb error: {e}")),
        }
    }

    fn display_name(&self) -> &str {
        "ADB"
    }
}
